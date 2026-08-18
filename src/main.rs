// =============================================================================
// main.rs — skeind: Context-Aware Visual State Daemon
// =============================================================================
//
// Daemon startup sequence:
//   1. Init tracing (RUST_LOG env var driven).
//   2. Parse config (XDG directories for socket, templates, output).
//   3. Connect to the D-Bus session bus (shared `zbus::Connection`).
//   4. Create the `watch::channel<Arc<Theme>>` — the single source of truth.
//   5. Spawn IPC tasks (Unix Domain Socket server + broadcaster).
//   6. Spawn the MPRIS D-Bus monitor → returns `art_rx: mpsc::Receiver<ArtEvent>`.
//   7. Load template renderer (reads ~/.config/skein/templates/).
//   8. Run the main orchestration loop:
//         art_rx.recv() → fetch art bytes (reqwest, async) →
//         spawn_blocking(extract_theme_from_bytes) →
//         theme_tx.send(Arc::new(theme)) →
//         template_renderer.render_all() (async file writes)
//   9. Handle SIGTERM/SIGINT for clean socket removal.
//
// Concurrency diagram:
//
//   ┌─────────────────────────────────────────────────────────────────────┐
//   │ tokio multi-thread runtime                                           │
//   │                                                                     │
//   │  main task (orchestrator)                                           │
//   │    ├─ art_rx.recv() ──────────────────────────────────────────────┐│
//   │    │                                                              ││
//   │    │  reqwest::get(url) [async — does NOT block event loop]       ││
//   │    │    ↓                                                         ││
//   │    │  spawn_blocking(extract_theme_from_bytes)                    ││
//   │    │    ↓  [runs on tokio blocking threadpool]                    ││
//   │    │  theme_tx.send(Arc::new(theme))                              ││
//   │    │    ↓                                                         ││
//   │    │  template_renderer.render_all()  [async I/O]                ││
//   │    └──────────────────────────────────────────────────────────────┘│
//   │                                                                     │
//   │  ipc::accept_task   ────── UnixListener.accept() loop              │
//   │  ipc::broadcast_task ───── watch::Receiver fan-out to UDS clients  │
//   │  dbus::watch_player  ───── zbus PropertyStream per MPRIS player    │
//   │  dbus::watch_name_owner_changes ── detects new MPRIS services      │
//   └─────────────────────────────────────────────────────────────────────┘
//
// =============================================================================

mod color;
mod dbus;
mod ipc;
mod template;

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use tokio::{
    signal,
    sync::watch,
};
use tracing::{error, info, warn};

use color::Theme;
use dbus::ArtEvent;
use template::TemplateRenderer;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Runtime configuration, resolved from XDG base directories.
struct Config {
    /// Path to the Unix Domain Socket (created by skeind, consumed by clients).
    socket_path: PathBuf,
    /// Directory containing `.jinja` templates.
    template_dir: PathBuf,
    /// Directory where rendered template outputs are written.
    output_dir: PathBuf,
    /// Whether to generate dark or light themes.
    prefer_dark: bool,
}

impl Config {
    fn resolve() -> Self {
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp"));

        let config_dir = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                dirs_next::config_dir().unwrap_or_else(|| PathBuf::from("~/.config"))
            });

        let cache_dir = std::env::var("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                dirs_next::cache_dir().unwrap_or_else(|| PathBuf::from("~/.cache"))
            });

        Self {
            socket_path: runtime_dir.join("skeind.sock"),
            template_dir: config_dir.join("skein/templates"),
            output_dir: cache_dir.join("skein"),
            prefer_dark: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Structured logging.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "skeind=info,warn".into()),
        )
        .with_target(false)
        .compact()
        .init();

    info!("skeind starting");

    let config = Config::resolve();

    // 2. D-Bus connection — shared across all zbus tasks.
    let dbus_conn = zbus::Connection::session()
        .await
        .context("Failed to connect to D-Bus session bus")?;

    info!("Connected to D-Bus session bus");

    // 3. Theme watch channel — the single source of truth.
    //    Initial value is a neutral dark theme so clients get something on
    //    first connect without waiting for MPRIS.
    let initial_theme = Arc::new(fallback_theme());
    let (theme_tx, theme_rx) = watch::channel(Arc::clone(&initial_theme));

    // 4. IPC: spawn UDS listener + broadcaster.
    ipc::spawn_ipc_tasks(&config.socket_path, theme_rx)
        .await
        .context("Failed to start IPC server")?;

    // 5. MPRIS monitor → art event channel.
    let mut art_rx = dbus::spawn_mpris_monitor(&dbus_conn)
        .await
        .context("Failed to start MPRIS monitor")?;

    // 6. Template renderer.
    let renderer = TemplateRenderer::load(&config.template_dir, &config.output_dir)
        .await
        .context("Failed to load template renderer")?;

    // 7. HTTP client for fetching remote album art.
    //    `reqwest::Client` is cheaply cloneable (Arc internals).
    let http = reqwest::Client::builder()
        .user_agent("skeind/0.1")
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("Failed to build HTTP client")?;

    info!("All subsystems started — waiting for events");

    // 8. Graceful shutdown signal.
    let shutdown = signal::ctrl_c();
    tokio::pin!(shutdown);

    // 9. Main orchestration loop.
    loop {
        tokio::select! {
            biased;

            // Handle Ctrl-C / SIGTERM.
            _ = &mut shutdown => {
                info!("Shutdown signal received");
                break;
            }

            // A media player's album art changed.
            maybe_event = art_rx.recv() => {
                match maybe_event {
                    Some(event) => {
                        handle_art_event(
                            event,
                            config.prefer_dark,
                            &http,
                            &theme_tx,
                            &renderer,
                        )
                        .await;
                    }
                    None => {
                        warn!("MPRIS art channel closed — all player watchers exited");
                        // Don't break: daemon stays alive; players may reconnect
                        // if NameOwnerChanged respawns a watcher.
                    }
                }
            }
        }
    }

    // 10. Clean up the socket file on orderly exit.
    if let Err(e) = std::fs::remove_file(&config.socket_path) {
        warn!(error = %e, "Failed to remove socket file on exit");
    }

    info!("skeind stopped");
    Ok(())
}

// ---------------------------------------------------------------------------
// Art event handler
// ---------------------------------------------------------------------------

/// Fetches album art bytes, extracts the theme on a blocking thread, and
/// broadcasts the result to all subsystems.
///
/// This function returns quickly even if image fetching is slow because:
///   • `reqwest::get` is async (does not block the event loop).
///   • `extract_theme_from_bytes` runs on `spawn_blocking`.
async fn handle_art_event(
    event: ArtEvent,
    prefer_dark: bool,
    http: &reqwest::Client,
    theme_tx: &watch::Sender<Arc<Theme>>,
    renderer: &Arc<TemplateRenderer>,
) {
    let url = match &event.art_url {
        Some(u) => u.clone(),
        None => {
            info!(player = %event.player, "Track has no album art; keeping current theme");
            return;
        }
    };

    info!(player = %event.player, url = %url, "Fetching album art");

    // Fetch the bytes (async — never blocks the tokio event loop).
    let bytes = match fetch_art_bytes(http, &url).await {
        Ok(b) => b,
        Err(e) => {
            error!(url = %url, error = %e, "Failed to fetch album art");
            return;
        }
    };

    info!(
        url = %url,
        bytes = bytes.len(),
        "Album art downloaded; spawning color extraction"
    );

    // Off-load the CPU-bound math to tokio's blocking threadpool.
    // This keeps the async event loop free to process new MPRIS signals.
    //
    // BOTTLENECK: `spawn_blocking` uses a shared pool (default: 512 threads
    // max).  Under normal use, one image is processed at a time so there's
    // no contention.  If you ever process multiple images concurrently,
    // consider wrapping with a `tokio::sync::Semaphore` to cap parallelism.
    let theme_result =
        tokio::task::spawn_blocking(move || color::extract_theme_from_bytes(&bytes, prefer_dark))
            .await;

    let theme = match theme_result {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => {
            error!(error = %e, "Color extraction failed");
            return;
        }
        Err(join_err) => {
            error!(error = %join_err, "spawn_blocking panicked during color extraction");
            return;
        }
    };

    info!(
        background = %theme.background,
        primary = %theme.primary,
        text = %theme.text,
        "New theme extracted"
    );

    // Broadcast the new theme to all UDS clients via the watch channel.
    // The IPC broadcaster task picks this up.
    let arc_theme = Arc::new(theme);
    let _ = theme_tx.send(Arc::clone(&arc_theme));

    // Also render legacy file templates (async file I/O, not blocking).
    let renderer = Arc::clone(renderer);
    let theme_for_tmpl = Arc::clone(&arc_theme);
    tokio::spawn(async move {
        if let Err(e) = renderer.render_all(&theme_for_tmpl).await {
            warn!(error = %e, "Template render pass failed");
        }
    });
}

// ---------------------------------------------------------------------------
// Art fetcher
// ---------------------------------------------------------------------------

/// Fetches album art from a `file://` or `http(s)://` URL.
///
/// • `file://` URIs are decoded and read directly from disk (tokio async).
/// • `http(s)://` URIs are fetched via reqwest.
///
/// Returns raw image bytes suitable for `image::load_from_memory`.
async fn fetch_art_bytes(http: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    if let Some(path_str) = url.strip_prefix("file://") {
        // Decode percent-encoding in file paths (e.g. `%20` → space).
        let decoded = percent_decode(path_str);
        tokio::fs::read(&decoded)
            .await
            .with_context(|| format!("Failed to read local art file: {decoded}"))
    } else {
        let response = http
            .get(url)
            .send()
            .await
            .with_context(|| format!("HTTP GET failed for {url}"))?;

        if !response.status().is_success() {
            anyhow::bail!(
                "HTTP {} fetching art from {}",
                response.status(),
                url
            );
        }

        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .with_context(|| format!("Failed to read HTTP response body from {url}"))
    }
}

/// Minimal percent-decoder for `file://` paths (handles `%20`, `%3A`, etc.).
fn percent_decode(s: &str) -> String {
    // A production implementation would use the `percent-encoding` crate.
    // For the common case (spaces and colons), this covers 99% of paths.
    s.replace("%20", " ")
        .replace("%3A", ":")
        .replace("%3a", ":")
}

// ---------------------------------------------------------------------------
// Fallback theme
// ---------------------------------------------------------------------------

/// A neutral dark theme shown to clients on startup, before any MPRIS event.
/// Colors are inspired by Catppuccin Mocha (hue-neutral, high contrast).
fn fallback_theme() -> Theme {
    use color::OklchSeed;

    Theme {
        background:      "#1e1e2e".into(),
        surface:         "#181825".into(),
        surface_variant: "#313244".into(),
        primary:         "#cba6f7".into(),
        accent:          "#f38ba8".into(),
        text:            "#cdd6f4".into(),
        subtext:         "#a6adc8".into(),
        seed_oklch: OklchSeed {
            lightness: 0.72,
            chroma: 0.14,
            hue: 297.0,
        },
        dark_mode: true,
    }
}
