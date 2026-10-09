use std::{
    path::{Path, PathBuf},
    sync::mpsc as std_mpsc,
};

use anyhow::{Context, Result};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

#[derive(Debug, Clone)]
pub struct WallpaperEvent {
    pub path: PathBuf,
}

pub async fn spawn_wallpaper_monitor(watch_dir: &Path) -> Result<mpsc::Receiver<WallpaperEvent>> {
    tokio::fs::create_dir_all(watch_dir)
        .await
        .with_context(|| format!("Failed to create wallpaper dir {}", watch_dir.display()))?;

    let (tx, rx) = mpsc::channel::<WallpaperEvent>(16);

    let initial_image = find_existing_wallpaper(watch_dir).await;
    if let Some(path) = initial_image {
        info!(path = %path.display(), "Found initial wallpaper on startup");
        let _ = tx.send(WallpaperEvent { path }).await;
    }

    let dir_to_watch = watch_dir.to_path_buf();
    let tx_clone = tx.clone();

    tokio::task::spawn_blocking(move || {
        let (std_tx, std_rx) = std_mpsc::channel::<Result<Event, notify::Error>>();

        let mut watcher = match RecommendedWatcher::new(
            move |res| {
                let _ = std_tx.send(res);
            },
            Config::default(),
        ) {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e, "Failed to create notify RecommendedWatcher");
                return;
            }
        };

        if let Err(e) = watcher.watch(&dir_to_watch, RecursiveMode::Recursive) {
            warn!(dir = %dir_to_watch.display(), error = %e, "Failed to watch wallpaper directory");
            return;
        }

        info!(dir = %dir_to_watch.display(), "Wallpaper directory watcher active");

        while let Ok(res) = std_rx.recv() {
            match res {
                Ok(event) => {
                    if is_relevant_event(&event) {
                        for path in event.paths {
                            if is_image_file(&path) {
                                debug!(path = %path.display(), "Wallpaper file event detected");
                                if tx_clone.blocking_send(WallpaperEvent { path }).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    debug!(error = %e, "Wallpaper watch error");
                }
            }
        }
    });

    Ok(rx)
}

async fn find_existing_wallpaper(dir: &Path) -> Option<PathBuf> {
    // Pick the most recently modified image, not the first directory entry
    // (read_dir order is arbitrary). A stable choice matters because the
    // daemon applies this theme immediately on startup.
    let mut entries = tokio::fs::read_dir(dir).await.ok()?;
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if !path.is_file() || !is_image_file(&path) {
            continue;
        }
        let mtime = entry
            .metadata()
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if newest.as_ref().is_none_or(|(t, _)| mtime > *t) {
            newest = Some((mtime, path));
        }
    }
    newest.map(|(_, path)| path)
}

fn is_relevant_event(event: &Event) -> bool {
    // Create/Modify only. Access events are deliberately ignored: tools that
    // merely *preview* files (thumbnailers, pickers) must not trigger a
    // re-extract. Explicit selection goes through the `set-wallpaper` IPC
    // command instead (see `skeinc set`).
    matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_)
    )
}

fn is_image_file(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_lowercase());

    matches!(
        ext.as_deref(),
        Some("png") | Some("jpg") | Some("jpeg") | Some("webp") | Some("bmp") | Some("gif")
    )
}
