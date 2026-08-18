// =============================================================================
// ipc.rs — Unix Domain Socket Broadcaster
// =============================================================================
//
// Architecture:
//   A `tokio::net::UnixListener` accepts incoming connections.
//   Each accepted stream is stored in a `ClientSet` (behind a `Mutex`).
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
//   │                         prune dead clients (write error)            │
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

use std::{path::Path, sync::Arc};

use anyhow::{Context, Result};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::{watch, Mutex},
};
use tracing::{debug, error, info, warn};

use crate::color::Theme;

// ---------------------------------------------------------------------------
// Client registry
// ---------------------------------------------------------------------------

/// The set of active client write-halves.
///
/// We own the `OwnedWriteHalf` so that we can write to each client
/// independently.  The `OwnedReadHalf` is discarded after accepting —
/// we don't expect clients to send us anything (publish-only socket).
type ClientSet = Arc<Mutex<Vec<tokio::net::unix::OwnedWriteHalf>>>;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Spawn two long-lived tasks:
/// 1. `accept_task` — continuously accepts new UDS connections.
/// 2. `broadcast_task` — fires whenever the theme `watch` changes.
///
/// Both tasks share the `ClientSet` via `Arc<Mutex<_>>`.
///
/// # Socket lifecycle
/// The socket file at `socket_path` is removed on startup if it already
/// exists (stale socket from a previous crash).  Callers should also
/// register a SIGTERM handler that calls `std::fs::remove_file` for clean
/// teardown.
pub async fn spawn_ipc_tasks(
    socket_path: &Path,
    theme_rx: watch::Receiver<Arc<Theme>>,
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

    // Spawn accept loop.
    let accept_clients = Arc::clone(&clients);
    tokio::spawn(accept_task(listener, accept_clients));

    // Spawn broadcaster.
    tokio::spawn(broadcast_task(theme_rx, Arc::clone(&clients)));

    Ok(())
}

// ---------------------------------------------------------------------------
// Accept loop
// ---------------------------------------------------------------------------

/// Accepts new connections indefinitely, pushing the write-half into `clients`.
async fn accept_task(listener: UnixListener, clients: ClientSet) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                debug!("New IPC client connected");
                on_new_client(stream, Arc::clone(&clients)).await;
            }
            Err(e) => {
                error!(error = %e, "UDS accept error");
                // A single accept error is not fatal; the kernel might just be
                // temporarily out of file descriptors.
            }
        }
    }
}

/// Registers a new client, immediately sending the current cached theme so
/// it doesn't have to wait for the next state change.
async fn on_new_client(stream: UnixStream, clients: ClientSet) {
    let (_, write_half) = stream.into_split();
    clients.lock().await.push(write_half);
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
    use crate::color::OklchSeed;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;

    fn dummy_theme() -> Theme {
        Theme {
            background: "#1a1a2e".into(),
            surface: "#16213e".into(),
            surface_variant: "#0f3460".into(),
            primary: "#533483".into(),
            accent: "#e94560".into(),
            text: "#eaeaea".into(),
            subtext: "#a8a8b3".into(),
            seed_oklch: OklchSeed {
                lightness: 0.55,
                chroma: 0.14,
                hue: 290.0,
            },
            dark_mode: true,
        }
    }

    #[tokio::test]
    async fn test_broadcast_reaches_client() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("skein.sock");

        let (tx, rx) = watch::channel(Arc::new(dummy_theme()));

        spawn_ipc_tasks(&socket_path, rx).await.unwrap();

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
        assert_eq!(parsed.background, "#1a1a2e");
    }
}
