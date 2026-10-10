// =============================================================================
// ipc.rs — Unix Domain Socket Broadcaster
// =============================================================================
//
// Architecture:
//   A `tokio::net::UnixListener` accepts incoming connections.
//   Each accepted stream contributes its write-half to a `ClientSet`
//   (behind a `Mutex`) and spawns a reader task for client→daemon messages.
//   When a new `Theme` arrives on the `watch::Receiver`, the broadcaster
//   task fans out the JSON payload to all currently-connected clients.
//
// Concurrency model:
//   ┌─────────────────────────────────────────────────────────────────────┐
//   │  ipc_server_task         ipc_broadcaster_task                       │
//   │  (accept loop)           (watch-driven fan-out)                     │
//   │                                                                     │
//   │  UnixListener            watch::Receiver<Arc<Theme>>                │
//   │       │                          │                                  │
//   │       ▼ new client               ▼ theme changed                   │
//   │  push OwnedWriteHalf    lock ClientSet                              │
//   │  into ClientSet         for each client: write JSON + '\n'          │
//   │  spawn reader task      prune dead clients (write error)            │
//   │  (patches + commands)                                               │
//   └─────────────────────────────────────────────────────────────────────┘
//
// PERFORMANCE NOTES:
//   • Fan-out holds the Mutex only for the duration of the lock+drain cycle.
//     Individual writes are not done under the lock; the owned halves are
//     drained into a local vec first.
//   • Dead clients (broken pipe) are pruned lazily on each broadcast —
//     no background health-check loop needed.
//   • For > ~100 simultaneous clients, consider switching ClientSet to a
//     `tokio::sync::broadcast` channel per client task to eliminate lock
//     contention entirely.
// =============================================================================

use std::{path::{Path, PathBuf}, sync::Arc};

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::{
    net::{UnixListener, UnixStream},
    sync::{watch, Mutex},
};
use tracing::{debug, error, info, warn};

use crate::color::{Scheme, Theme};
use crate::template::TemplateRenderer;


// ---------------------------------------------------------------------------
// Client registry
// ---------------------------------------------------------------------------

/// Commands a client can send to the daemon (one JSON object per line).
///
/// `Theme` patches (sent by `skeinc` tweaking) are still accepted as bare
/// theme objects. Commands use a tagged envelope so the two never collide.
#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum ClientCommand {
    /// Re-extract the theme from an explicit image path and broadcast it.
    /// This is the handoff for wallpaper pickers: instead of pointing the
    /// daemon at a directory full of previews (which would spam
    /// re-extracts), the picker calls `skeinc set <path>` once the user
    /// confirms a choice.
    SetWallpaper {
        path: PathBuf,
    },
}

/// The set of active client write-halves.
///
/// We own the `OwnedWriteHalf` so that we can write to each client
/// independently.
type ClientSet = Arc<Mutex<Vec<tokio::net::unix::OwnedWriteHalf>>>;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Reader/writer configuration captured for command handling.
#[derive(Debug, Clone)]
struct CommandCtx {
    prefer_dark: bool,
    scheme: Scheme,
    contrast: f32,
    output_dir: PathBuf,
}

/// Static configuration for the IPC server (extraction parameters used when
/// executing client commands such as `set-wallpaper`).
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Directory receiving rendered templates and `theme.json`.
    pub output_dir: PathBuf,
    /// Color mode for command-triggered extractions.
    pub prefer_dark: bool,
    /// Palette scheme for command-triggered extractions.
    pub scheme: Scheme,
    /// Text contrast target for command-triggered extractions.
    pub contrast: f32,
}

/// Spawn two long-lived tasks:
/// 1. `accept_task` — continuously accepts new UDS connections.
/// 2. `broadcast_task` — fires whenever the theme `watch` changes.
///
/// Clients may also send commands (see [`ClientCommand`]); accepted themes
/// and command results are fanned out through the same `watch` channel.
///
/// # Socket lifecycle
/// The socket file at `socket_path` is removed on startup if it already
/// exists (stale socket from a previous crash).  Callers should also
/// register a SIGTERM handler that calls `std::fs::remove_file` for clean
/// teardown.
pub async fn spawn_ipc_tasks(
    socket_path: &Path,
    theme_tx: watch::Sender<Arc<Theme>>,
    theme_rx: watch::Receiver<Arc<Theme>>,
    renderer: Arc<TemplateRenderer>,
    server: ServerConfig,
) -> Result<()> {
    // Remove a stale socket file if present.
    if socket_path.exists() {
        std::fs::remove_file(socket_path).with_context(|| {
            format!("Failed to remove stale socket at {}", socket_path.display())
        })?;
    }

    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("Failed to bind UDS at {}", socket_path.display()))?;

    info!(socket = %socket_path.display(), "IPC socket listening");

    let clients: ClientSet = Arc::new(Mutex::new(Vec::new()));
    let cmd_ctx = CommandCtx {
        prefer_dark: server.prefer_dark,
        scheme: server.scheme,
        contrast: server.contrast,
        output_dir: server.output_dir,
    };

    // Spawn accept loop.
    let accept_clients = Arc::clone(&clients);
    tokio::spawn(accept_task(
        listener,
        accept_clients,
        theme_tx,
        theme_rx.clone(),
        renderer,
        cmd_ctx,
    ));

    // Spawn broadcaster.
    tokio::spawn(broadcast_task(theme_rx, Arc::clone(&clients)));

    Ok(())
}

// ---------------------------------------------------------------------------
// Accept loop
// ---------------------------------------------------------------------------

/// Accepts new connections indefinitely, sending the current state and pushing
/// the write-half into `clients`.
async fn accept_task(
    listener: UnixListener,
    clients: ClientSet,
    theme_tx: watch::Sender<Arc<Theme>>,
    theme_rx: watch::Receiver<Arc<Theme>>,
    renderer: Arc<TemplateRenderer>,
    cmd_ctx: CommandCtx,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                debug!("New IPC client connected");
                on_new_client(
                    stream,
                    Arc::clone(&clients),
                    &theme_tx,
                    &theme_rx,
                    &renderer,
                    cmd_ctx.clone(),
                )
                .await;
            }
            Err(e) => {
                error!(error = %e, "UDS accept error");
            }
        }
    }
}

/// Registers a new client, immediately sending the current cached theme,
/// spawning a reader task for client patches, and saving write_half to clients.
async fn on_new_client(
    stream: UnixStream,
    clients: ClientSet,
    theme_tx: &watch::Sender<Arc<Theme>>,
    theme_rx: &watch::Receiver<Arc<Theme>>,
    renderer: &Arc<TemplateRenderer>,
    cmd_ctx: CommandCtx,
) {
    use tokio::io::AsyncWriteExt;

    let (read_half, mut write_half) = stream.into_split();

    // Send current cached theme immediately on connection.
    let theme = Arc::clone(&*theme_rx.borrow());
    if let Ok(mut json) = serialize_theme(&theme) {
        json.push(b'\n');
        let _ = write_half.write_all(&json).await;
        let _ = write_half.flush().await;
    }

    // Spawn reader task to receive patches and commands pushed from clients.
    tokio::spawn(client_read_task(
        read_half,
        theme_tx.clone(),
        Arc::clone(renderer),
        cmd_ctx,
    ));

    clients.lock().await.push(write_half);
}

/// Reads incoming JSON lines from a client:
///   * bare `Theme` objects (e.g. skeinc tweaking state) are applied directly;
///   * [`ClientCommand`] envelopes (e.g. `set-wallpaper`) are executed and
///     their results broadcast like any other theme change.
async fn client_read_task(
    read_half: tokio::net::unix::OwnedReadHalf,
    theme_tx: watch::Sender<Arc<Theme>>,
    renderer: Arc<TemplateRenderer>,
    cmd_ctx: CommandCtx,
) {
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(read_half).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(theme) = serde_json::from_str::<Theme>(&line) {
            let arc_theme = Arc::new(theme);
            info!(
                bg = %arc_theme.background,
                primary = %arc_theme.primary,
                "Received theme patch from IPC client"
            );
            apply_theme(arc_theme, &theme_tx, &renderer, &cmd_ctx.output_dir);
            continue;
        }
        match serde_json::from_str::<ClientCommand>(&line) {
            Ok(ClientCommand::SetWallpaper { path }) => {
                handle_set_wallpaper(path, &theme_tx, &renderer, &cmd_ctx).await;
            }
            Err(e) => {
                debug!(error = %e, "Ignoring unparsable IPC line");
            }
        }
    }
}

/// Publish a theme to subscribers, re-render templates, and refresh the
/// `theme.json` snapshot — the single fan-out path for every theme change.
fn apply_theme(
    theme: Arc<Theme>,
    theme_tx: &watch::Sender<Arc<Theme>>,
    renderer: &Arc<TemplateRenderer>,
    output_dir: &Path,
) {
    let _ = theme_tx.send(Arc::clone(&theme));
    let renderer = Arc::clone(renderer);
    let output_dir = output_dir.to_path_buf();
    tokio::spawn(async move {
        if let Err(e) = renderer.render_all(&theme).await {
            warn!(error = %e, "Template render pass failed");
        }
        if let Err(e) = crate::template::write_snapshot(&output_dir, &theme).await {
            warn!(error = %e, "Theme snapshot write failed");
        }
    });
}

/// Execute `set-wallpaper`: read + extract the image at `path`, then fan the
/// resulting theme out through the normal broadcast path.
async fn handle_set_wallpaper(
    path: PathBuf,
    theme_tx: &watch::Sender<Arc<Theme>>,
    renderer: &Arc<TemplateRenderer>,
    cmd_ctx: &CommandCtx,
) {
    info!(path = %path.display(), "IPC set-wallpaper request");
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "set-wallpaper: failed to read image");
            return;
        }
    };
    let CommandCtx {
        prefer_dark,
        scheme,
        contrast,
        ..
    } = *cmd_ctx;
    let theme_res = tokio::task::spawn_blocking(move || {
        crate::color::extract_theme_from_bytes(&bytes, prefer_dark, scheme, contrast)
    })
    .await;
    match theme_res {
        Ok(Ok(theme)) => {
            let arc_theme = Arc::new(theme);
            info!(bg = %arc_theme.background, primary = %arc_theme.primary, "Applying IPC wallpaper theme");
            apply_theme(arc_theme, theme_tx, renderer, &cmd_ctx.output_dir);
        }
        Ok(Err(e)) => warn!(error = %e, "set-wallpaper: theme extraction failed"),
        Err(e) => warn!(error = %e, "set-wallpaper: extraction task panicked"),
    }
}

// ---------------------------------------------------------------------------
// Broadcaster
// ---------------------------------------------------------------------------

/// Watches the theme channel and broadcasts JSON to all clients on change.
async fn broadcast_task(mut theme_rx: watch::Receiver<Arc<Theme>>, clients: ClientSet) {
    // Wait for the very first theme before starting to broadcast.
    loop {
        // `changed()` completes when a new value is sent *after* the last borrow.
        if theme_rx.changed().await.is_err() {
            // The sender was dropped — daemon is shutting down.
            info!("Theme watch channel closed; IPC broadcaster exiting");
            return;
        }

        let theme = Arc::clone(&*theme_rx.borrow_and_update());

        // Serialize once, reuse the bytes for all clients.
        let json = match serialize_theme(&theme) {
            Ok(j) => j,
            Err(e) => {
                error!(error = %e, "Failed to serialize theme — skipping broadcast");
                continue;
            }
        };

        broadcast_to_all(&json, Arc::clone(&clients)).await;
    }
}

/// Fan-out: write `payload\n` to every connected client.
/// Dead clients (broken pipe / OS error) are pruned from the set.
async fn broadcast_to_all(payload: &[u8], clients: ClientSet) {
    use tokio::io::AsyncWriteExt;

    let mut guard = clients.lock().await;

    // Drain the vec into a local buffer, write concurrently, then re-insert
    // the ones that are still alive.  This avoids holding the lock during I/O.
    let mut live: Vec<tokio::net::unix::OwnedWriteHalf> = Vec::with_capacity(guard.len());
    let mut dead = 0usize;

    for mut client in guard.drain(..) {
        // Each write is: JSON payload + newline delimiter.
        // Clients split on newlines to reconstruct messages.
        let write_result = async {
            client.write_all(payload).await?;
            client.write_all(b"\n").await?;
            client.flush().await?;
            Ok::<_, std::io::Error>(())
        }
        .await;

        match write_result {
            Ok(()) => live.push(client),
            Err(e) => {
                dead += 1;
                debug!(error = %e, "Pruning dead IPC client");
            }
        }
    }

    if dead > 0 {
        warn!(pruned = dead, remaining = live.len(), "Pruned dead IPC clients");
    }

    *guard = live;
}

// ---------------------------------------------------------------------------
// Serialization
// ---------------------------------------------------------------------------

/// Serialize a `Theme` into a compact JSON line (no trailing newline).
fn serialize_theme(theme: &Theme) -> Result<Vec<u8>> {
    serde_json::to_vec(theme).context("Theme JSON serialization failed")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;

    fn dummy_theme() -> Theme {
        Theme::test_fallback()
    }

    #[tokio::test]
    async fn test_broadcast_reaches_client() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("skein.sock");

        let (tx, rx) = watch::channel(Arc::new(dummy_theme()));
        let tmpl_dir = dir.path().join("tmpl");
        let out_dir = dir.path().join("out");
        let renderer = TemplateRenderer::load(&tmpl_dir, &out_dir, None).await.unwrap();

        spawn_ipc_tasks(
            &socket_path,
            tx.clone(),
            rx,
            renderer,
            ServerConfig {
                output_dir: out_dir.clone(),
                prefer_dark: true,
                scheme: Scheme::Tonal,
                contrast: 4.5,
            },
        )
        .await
        .unwrap();

        // Give the accept task time to start.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Connect a client.
        let mut client = tokio::net::UnixStream::connect(&socket_path).await.unwrap();

        // Send a new theme.
        tx.send(Arc::new(dummy_theme())).unwrap();

        // Read the broadcast with a timeout.
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut tmp = [0u8; 4096];
            loop {
                let n = client.read(&mut tmp).await.unwrap();
                buf.extend_from_slice(&tmp[..n]);
                if buf.contains(&b'\n') {
                    break;
                }
            }
        })
        .await
        .expect("Broadcast did not arrive within 2 s");

        // Validate the JSON.
        let line = buf.split(|&b| b == b'\n').next().unwrap();
        let parsed: Theme = serde_json::from_slice(line).unwrap();
        assert_eq!(parsed.background, "#1e1e2e");
    }

    #[tokio::test]
    async fn test_client_patch_updates_daemon() {
        use tokio::io::AsyncWriteExt;

        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("skein.sock");

        let (tx, mut rx) = watch::channel(Arc::new(dummy_theme()));
        let tmpl_dir = dir.path().join("tmpl");
        let out_dir = dir.path().join("out");
        let renderer = TemplateRenderer::load(&tmpl_dir, &out_dir, None).await.unwrap();

        spawn_ipc_tasks(
            &socket_path,
            tx.clone(),
            rx.clone(),
            renderer,
            ServerConfig {
                output_dir: out_dir.clone(),
                prefer_dark: true,
                scheme: Scheme::Tonal,
                contrast: 4.5,
            },
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = tokio::net::UnixStream::connect(&socket_path).await.unwrap();

        // Client pushes a patched theme.
        let mut patched = dummy_theme();
        patched.background = "#001122".into();
        let mut json = serde_json::to_vec(&patched).unwrap();
        json.push(b'\n');

        client.write_all(&json).await.unwrap();
        client.flush().await.unwrap();

        // Wait for daemon's theme_rx to update.
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                rx.changed().await.unwrap();
                let theme = rx.borrow();
                if theme.background == "#001122" {
                    break;
                }
            }
        })
        .await
        .expect("Daemon watch channel did not receive client patch within 2s");
    }

    #[tokio::test]
    async fn test_set_wallpaper_command_updates_daemon() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("skein.sock");

        let (tx, mut rx) = watch::channel(Arc::new(dummy_theme()));
        let tmpl_dir = dir.path().join("tmpl");
        let out_dir = dir.path().join("out");
        let renderer = TemplateRenderer::load(&tmpl_dir, &out_dir, None).await.unwrap();

        spawn_ipc_tasks(
            &socket_path,
            tx.clone(),
            rx.clone(),
            renderer,
            ServerConfig {
                output_dir: out_dir.clone(),
                prefer_dark: true,
                scheme: Scheme::Tonal,
                contrast: 4.5,
            },
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // A solid-red still life as the "wallpaper".
        let img_path = dir.path().join("wall.png");
        image::RgbImage::from_pixel(8, 8, image::Rgb([200, 40, 60]))
            .save(&img_path)
            .unwrap();

        let client = tokio::net::UnixStream::connect(&socket_path).await.unwrap();

        // Discard the cached-theme line the server sends on connect.
        let mut lines = tokio::io::BufReader::new(client).lines();
        lines.next_line().await.unwrap();
        let mut client = lines.into_inner();

        let cmd = serde_json::json!({"cmd": "set-wallpaper", "path": img_path}).to_string();
        client.write_all(cmd.as_bytes()).await.unwrap();
        client.write_all(b"\n").await.unwrap();
        client.flush().await.unwrap();

        // Wait for the extracted theme (background must change).
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                rx.changed().await.unwrap();
                if rx.borrow().background != "#1a1a2e" {
                    break;
                }
            }
        })
        .await
        .expect("Daemon did not apply set-wallpaper within 5s");

        // Snapshot file was refreshed as well.
        tokio::time::timeout(Duration::from_secs(5), async {
            while !out_dir.join("theme.json").exists() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("theme.json snapshot was not written");
    }
}
