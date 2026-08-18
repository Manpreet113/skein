// =============================================================================
// dbus.rs — MPRIS D-Bus Listener via zbus
// =============================================================================
//
// This module monitors all running MPRIS-compliant media players and triggers
// theme extraction whenever album art changes.
//
// MPRIS2 D-Bus layout:
//   service:   org.mpris.MediaPlayer2.<player>  (e.g. spotify, mpd, vlc)
//   path:      /org/mpris/MediaPlayer2
//   interface: org.mpris.MediaPlayer2.Player
//   property:  Metadata  →  mpris:artUrl  (string: file:// or http:// URI)
//
// Strategy:
//   1. On startup, enumerate all MPRIS services currently on the session bus.
//   2. For each player found (and each new player that appears), spawn an
//      async task that streams PropertiesChanged signals via a generated proxy.
//   3. When `mpris:artUrl` changes, send the URL through the `art_url_tx`
//      channel; the main orchestrator picks it up and calls spawn_blocking.
//
// PERFORMANCE NOTES:
//   • One zbus connection is shared across all player tasks (Connection::clone
//     is cheap — it's an Arc internally).
//   • NameOwnerChanged monitoring lets us track players appearing/disappearing
//     without polling.
// =============================================================================

use std::collections::HashSet;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use zbus::{fdo, Connection};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Event emitted when album art changes on any MPRIS player.
#[derive(Debug, Clone)]
pub struct ArtEvent {
    /// The player that emitted the change (e.g. `org.mpris.MediaPlayer2.spotify`).
    pub player: String,
    /// The new art URL (`file://...` or `http://...`).  `None` if the track
    /// has no artwork (e.g. a podcast or radio stream).
    pub art_url: Option<String>,
}

// ---------------------------------------------------------------------------
// MPRIS proxy definition
// ---------------------------------------------------------------------------
// The `#[zbus::proxy]` attribute macro generates:
//   • `MediaPlayer2PlayerProxy<'_>` struct
//   • `.metadata()` async getter
//   • `.receive_metadata_changed()` async stream constructor

#[zbus::proxy(
    interface = "org.mpris.MediaPlayer2.Player",
    default_path = "/org/mpris/MediaPlayer2"
)]
trait MediaPlayer2Player {
    /// The `Metadata` property: a dict of track information.
    /// We specifically look for `mpris:artUrl`.
    #[zbus(property)]
    fn metadata(
        &self,
    ) -> zbus::Result<std::collections::HashMap<String, zbus::zvariant::OwnedValue>>;
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Starts the MPRIS monitor and returns a channel receiver that emits
/// [`ArtEvent`] whenever album art changes on any running player.
///
/// Internally spawns:
/// - One task that watches the D-Bus `NameOwnerChanged` signal.
/// - One task per detected media player that streams `PropertiesChanged`.
///
/// The caller must drive the returned receiver:
/// ```rust,ignore
/// while let Some(event) = art_rx.recv().await {
///     // trigger palette extraction
/// }
/// ```
pub async fn spawn_mpris_monitor(conn: &Connection) -> Result<mpsc::Receiver<ArtEvent>> {
    let (tx, rx) = mpsc::channel::<ArtEvent>(16);

    // Enumerate already-running players.
    let initial_players = list_mpris_players(conn).await?;
    info!(count = initial_players.len(), "Found MPRIS players at startup");

    let mut tracked: HashSet<String> = HashSet::new();

    for player in &initial_players {
        if tracked.insert(player.clone()) {
            tokio::spawn(watch_player(conn.clone(), player.clone(), tx.clone()));
        }
    }

    // Watch for new players appearing on the bus.
    tokio::spawn(watch_name_owner_changes(conn.clone(), tx, tracked));

    Ok(rx)
}

// ---------------------------------------------------------------------------
// Enumerate active MPRIS players
// ---------------------------------------------------------------------------

async fn list_mpris_players(conn: &Connection) -> Result<Vec<String>> {
    let dbus_proxy = fdo::DBusProxy::new(conn)
        .await
        .context("Failed to create D-Bus proxy")?;

    let names = dbus_proxy
        .list_names()
        .await
        .context("Failed to list D-Bus names")?;

    let players: Vec<String> = names
        .into_iter()
        .filter(|n| n.starts_with("org.mpris.MediaPlayer2."))
        .map(|n| n.to_string())
        .collect();

    Ok(players)
}

// ---------------------------------------------------------------------------
// Track a single player's metadata changes
// ---------------------------------------------------------------------------

/// Subscribes to `Metadata` property changes for a specific MPRIS player.
/// Emits [`ArtEvent`] on `tx` whenever `mpris:artUrl` changes.
async fn watch_player(conn: Connection, player_name: String, tx: mpsc::Sender<ArtEvent>) {
    info!(player = %player_name, "Starting MPRIS watch");

    let proxy = match MediaPlayer2PlayerProxy::builder(&conn)
        .destination(player_name.as_str())
    {
        Ok(builder) => match builder.build().await {
            Ok(p) => p,
            Err(e) => {
                warn!(player = %player_name, error = %e, "Failed to build MPRIS proxy");
                return;
            }
        },
        Err(e) => {
            warn!(player = %player_name, error = %e, "Failed to set proxy destination");
            return;
        }
    };

    // Get a stream of changes to the `Metadata` property.
    // `receive_metadata_changed()` is generated by the `#[zbus::proxy]` macro.
    let mut metadata_stream = match proxy.receive_metadata_changed().await {
        stream => stream,
    };

    // Track the last seen art URL to avoid redundant extractions.
    let mut last_art_url: Option<String> = None;

    while let Some(change) = metadata_stream.next().await {
        let metadata = match change.get().await {
            Ok(m) => m,
            Err(e) => {
                debug!(error = %e, "Failed to read Metadata value");
                continue;
            }
        };

        let art_url = extract_art_url(&metadata);

        if art_url == last_art_url {
            // URL hasn't actually changed (e.g. seek event also fires PropertiesChanged).
            continue;
        }

        debug!(
            player = %player_name,
            url = ?art_url,
            "Album art URL changed"
        );

        last_art_url = art_url.clone();

        let event = ArtEvent {
            player: player_name.clone(),
            art_url,
        };

        if tx.send(event).await.is_err() {
            // Receiver dropped — daemon is shutting down.
            break;
        }
    }

    info!(player = %player_name, "MPRIS watch ended");
}

// ---------------------------------------------------------------------------
// Watch for new players appearing / disappearing
// ---------------------------------------------------------------------------

/// Monitors `org.freedesktop.DBus.NameOwnerChanged` signals to detect new
/// MPRIS players that start after daemon launch.
async fn watch_name_owner_changes(
    conn: Connection,
    tx: mpsc::Sender<ArtEvent>,
    mut tracked: HashSet<String>,
) {
    let dbus_proxy = match fdo::DBusProxy::new(&conn).await {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "Failed to create D-Bus proxy for NameOwnerChanged");
            return;
        }
    };

    let mut name_stream = match dbus_proxy.receive_name_owner_changed().await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "Failed to subscribe to NameOwnerChanged");
            return;
        }
    };

    while let Some(signal) = name_stream.next().await {
        let args = match signal.args() {
            Ok(a) => a,
            Err(_) => continue,
        };

        let name = args.name.to_string();

        // Only care about MPRIS services.
        if !name.starts_with("org.mpris.MediaPlayer2.") {
            continue;
        }

        let new_owner = args.new_owner.as_deref().unwrap_or("");
        let old_owner = args.old_owner.as_deref().unwrap_or("");

        if !new_owner.is_empty() && old_owner.is_empty() {
            // New player appeared.
            if tracked.insert(name.clone()) {
                info!(player = %name, "New MPRIS player detected");
                tokio::spawn(watch_player(conn.clone(), name, tx.clone()));
            }
        } else if new_owner.is_empty() && !old_owner.is_empty() {
            // Player disappeared.
            tracked.remove(&name);
            debug!(player = %name, "MPRIS player vanished");
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extracts the `mpris:artUrl` string from a Metadata dict.
fn extract_art_url(
    metadata: &std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
) -> Option<String> {
    metadata
        .get("mpris:artUrl")
        .and_then(|v| {
            // OwnedValue may be a zvariant::Value::Str or wrapped Str variant.
            // Try to coerce to &str via the PartialEq<str> impl.
            match v.downcast_ref::<zbus::zvariant::Str>() {
                Ok(s) => {
                    let url = s.as_str().trim().to_owned();
                    if url.is_empty() {
                        None
                    } else {
                        Some(url)
                    }
                }
                Err(_) => None,
            }
        })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_art_url_present() {
        use zbus::zvariant::{OwnedValue, Value};
        let mut map = std::collections::HashMap::new();
        let url = "file:///home/user/.cache/mpd/albumart.jpg";
        // zvariant 5.x: OwnedValue implements TryFrom<Value<'_>>, not From.
        let owned: OwnedValue = Value::from(url).try_into().unwrap();
        map.insert("mpris:artUrl".to_string(), owned);
        assert_eq!(extract_art_url(&map), Some(url.to_string()));
    }

    #[test]
    fn extract_art_url_missing() {
        let map = std::collections::HashMap::new();
        assert_eq!(extract_art_url(&map), None);
    }

    #[test]
    fn extract_art_url_empty_string() {
        use zbus::zvariant::{OwnedValue, Value};
        let mut map = std::collections::HashMap::new();
        let owned: OwnedValue = Value::from("").try_into().unwrap();
        map.insert("mpris:artUrl".to_string(), owned);
        assert_eq!(extract_art_url(&map), None);
    }
}
