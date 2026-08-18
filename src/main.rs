mod color;
mod dbus;
mod ipc;
mod template;
mod wallpaper;

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use tokio::{signal, sync::watch};
use tracing::{info, warn};

use color::Theme;
use template::TemplateRenderer;


struct Config {
    socket_path: PathBuf,
    template_dir: PathBuf,
    wallpaper_dir: PathBuf,
    output_dir: PathBuf,
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
            wallpaper_dir: config_dir.join("skein/wallpaper"),
            output_dir: cache_dir.join("skein"),
            prefer_dark: true,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
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

    let dbus_conn = zbus::Connection::session()
        .await
        .context("Failed to connect to D-Bus session bus")?;

    info!("Connected to D-Bus session bus");

    let initial_theme = Arc::new(fallback_theme());
    let (theme_tx, theme_rx) = watch::channel(Arc::clone(&initial_theme));

    let renderer = TemplateRenderer::load(&config.template_dir, &config.output_dir)
        .await
        .context("Failed to load template renderer")?;

    ipc::spawn_ipc_tasks(
        &config.socket_path,
        theme_tx.clone(),
        theme_rx,
        Arc::clone(&renderer),
    )
    .await
    .context("Failed to start IPC server")?;

    let mut art_rx = dbus::spawn_mpris_monitor(&dbus_conn)
        .await
        .context("Failed to start MPRIS monitor")?;

    let mut wallpaper_rx = wallpaper::spawn_wallpaper_monitor(&config.wallpaper_dir)
        .await
        .context("Failed to start wallpaper monitor")?;

    let http = reqwest::Client::builder()
        .user_agent("skeind/0.1")
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("Failed to build HTTP client")?;

    info!("All subsystems started — waiting for events");

    let shutdown = signal::ctrl_c();
    tokio::pin!(shutdown);

    let mut baseline_theme: Option<Arc<Theme>> = None;
    let mut mpris_active = false;

    loop {
        tokio::select! {
            biased;

            _ = &mut shutdown => {
                info!("Shutdown signal received");
                break;
            }

            maybe_wall = wallpaper_rx.recv() => {
                if let Some(event) = maybe_wall {
                    info!(path = %event.path.display(), "Wallpaper event triggered");
                    if let Ok(bytes) = tokio::fs::read(&event.path).await {
                        let prefer_dark = config.prefer_dark;
                        let theme_res = tokio::task::spawn_blocking(move || {
                            color::extract_theme_from_bytes(&bytes, prefer_dark)
                        })
                        .await;

                        if let Ok(Ok(theme)) = theme_res {
                            let arc_theme = Arc::new(theme);
                            baseline_theme = Some(Arc::clone(&arc_theme));

                            if !mpris_active {
                                info!(bg = %arc_theme.background, primary = %arc_theme.primary, "Applying wallpaper baseline theme");
                                broadcast_and_render(&arc_theme, &theme_tx, &renderer);
                            } else {
                                info!("Wallpaper baseline updated in background (MPRIS active)");
                            }
                        }
                    }
                }
            }

            maybe_event = art_rx.recv() => {
                match maybe_event {
                    Some(event) => {
                        if let Some(url) = &event.art_url {
                            info!(player = %event.player, url = %url, "Fetching album art");
                            if let Ok(bytes) = fetch_art_bytes(&http, url).await {
                                let prefer_dark = config.prefer_dark;
                                let theme_res = tokio::task::spawn_blocking(move || {
                                    color::extract_theme_from_bytes(&bytes, prefer_dark)
                                })
                                .await;

                                if let Ok(Ok(theme)) = theme_res {
                                    let arc_theme = Arc::new(theme);
                                    mpris_active = true;
                                    info!(bg = %arc_theme.background, primary = %arc_theme.primary, "Applying transient MPRIS album art theme");
                                    broadcast_and_render(&arc_theme, &theme_tx, &renderer);
                                }
                            }
                        } else {
                            mpris_active = false;
                            let revert_theme = baseline_theme.clone().unwrap_or_else(|| Arc::new(fallback_theme()));
                            info!(bg = %revert_theme.background, "MPRIS playback stopped/no art — reverting to baseline theme");
                            broadcast_and_render(&revert_theme, &theme_tx, &renderer);
                        }
                    }
                    None => {
                        warn!("MPRIS art channel closed");
                    }
                }
            }
        }
    }

    if let Err(e) = std::fs::remove_file(&config.socket_path) {
        warn!(error = %e, "Failed to remove socket file on exit");
    }

    info!("skeind stopped");
    Ok(())
}

fn broadcast_and_render(
    theme: &Arc<Theme>,
    theme_tx: &watch::Sender<Arc<Theme>>,
    renderer: &Arc<TemplateRenderer>,
) {
    let _ = theme_tx.send(Arc::clone(theme));
    let renderer = Arc::clone(renderer);
    let theme_for_tmpl = Arc::clone(theme);
    tokio::spawn(async move {
        if let Err(e) = renderer.render_all(&theme_for_tmpl).await {
            warn!(error = %e, "Template render pass failed");
        }
    });
}

async fn fetch_art_bytes(http: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    if let Some(path_str) = url.strip_prefix("file://") {
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
            anyhow::bail!("HTTP {} fetching art from {}", response.status(), url);
        }

        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .with_context(|| format!("Failed to read HTTP response body from {url}"))
    }
}

fn percent_decode(s: &str) -> String {
    s.replace("%20", " ")
        .replace("%3A", ":")
        .replace("%3a", ":")
}

fn fallback_theme() -> Theme {
    use color::OklchSeed;

    Theme {
        background: "#1e1e2e".into(),
        surface: "#181825".into(),
        surface_variant: "#313244".into(),
        primary: "#cba6f7".into(),
        accent: "#f38ba8".into(),
        text: "#cdd6f4".into(),
        subtext: "#a6adc8".into(),
        seed_oklch: OklchSeed {
            lightness: 0.72,
            chroma: 0.14,
            hue: 297.0,
        },
        dark_mode: true,
    }
}
