use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use clap::Parser;
use tokio::{signal, sync::watch};
use tracing::{info, warn};

use skein::color::{self, Scheme, Theme};
use skein::template::{self, TemplateRenderer};
use skein::{ipc, wallpaper};

#[derive(Parser, Debug)]
#[command(name = "skeind", version, about = "Wallpaper-driven color theme daemon")]
struct Args {
    /// Run once against a single image, render templates, and exit.
    #[arg(long, value_name = "PATH")]
    image: Option<PathBuf>,

    /// Print the extracted theme as JSON (with --image) and exit.
    #[arg(long)]
    json: bool,

    /// Color mode.
    #[arg(long, value_enum)]
    mode: Option<Mode>,

    /// Explicit seed color ("#RRGGBB") instead of extracting from the image.
    #[arg(long)]
    seed: Option<String>,

    /// Minimum text contrast ratio against the background (e.g. 4.5 or 7.0).
    #[arg(long)]
    contrast: Option<f32>,

    /// Palette scheme.
    #[arg(long, value_enum)]
    scheme: Option<Scheme>,

    /// Directory of wallpaper images to watch.
    #[arg(long)]
    wallpaper_dir: Option<PathBuf>,

    /// Directory containing *.jinja templates.
    #[arg(long)]
    template_dir: Option<PathBuf>,

    /// Directory where rendered templates are written.
    #[arg(long)]
    output_dir: Option<PathBuf>,

    /// Unix socket path for IPC.
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Path to config file (default: ~/.config/skein/config.toml).
    #[arg(long)]
    config: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Mode {
    Dark,
    Light,
    Auto,
}

/// Resolve the effective dark/light choice from CLI, config, or the system.
fn resolve_prefer_dark(cli: Option<Mode>, file_mode: Option<&str>) -> bool {
    let mode = cli.or(match file_mode {
        Some("light") => Some(Mode::Light),
        Some("dark") => Some(Mode::Dark),
        Some("auto") => Some(Mode::Auto),
        _ => None,
    });

    match mode {
        Some(Mode::Light) => false,
        Some(Mode::Dark) => true,
        Some(Mode::Auto) | None => color::system_prefers_dark(),
    }
}

#[derive(Debug, Default, serde::Deserialize)]
struct FileConfig {
    wallpaper_dir: Option<PathBuf>,
    template_dir: Option<PathBuf>,
    output_dir: Option<PathBuf>,
    socket: Option<PathBuf>,
    mode: Option<String>,
    scheme: Option<String>,
    seed: Option<String>,
    contrast: Option<f32>,
}

/// Expand a leading `~` or `~/` to the user's home directory.
fn expand_tilde(p: PathBuf) -> PathBuf {
    match p.to_str() {
        Some(s) if s == "~" => dirs_next::home_dir().unwrap_or(p),
        Some(s) if s.starts_with("~/") => {
            match dirs_next::home_dir() {
                Some(home) => home.join(&s[2..]),
                None => p,
            }
        }
        _ => p,
    }
}

struct Config {
    socket_path: PathBuf,
    template_dir: PathBuf,
    wallpaper_dir: PathBuf,
    output_dir: PathBuf,
    prefer_dark: bool,
    scheme: Scheme,
    seed: Option<String>,
    contrast: f32,
}

impl Config {
    fn resolve(args: &Args) -> Result<Self> {
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

        let config_path = args
            .config
            .clone()
            .unwrap_or_else(|| config_dir.join("skein/config.toml"));

        let file = match std::fs::read_to_string(&config_path) {
            Ok(raw) => toml::from_str::<FileConfig>(&raw)
                .with_context(|| format!("Failed to parse {}", config_path.display()))?,
            Err(_) => FileConfig::default(),
        };

        let prefer_dark = resolve_prefer_dark(args.mode, file.mode.as_deref());

        let scheme = match (args.scheme, file.scheme.as_deref()) {
            (Some(s), _) => s,
            (None, Some(s)) => s.parse()?,
            (None, None) => Scheme::Tonal,
        };

        let seed = args.seed.clone().or(file.seed);

        let contrast = args
            .contrast
            .or(file.contrast)
            .unwrap_or(4.5_f32)
            .clamp(1.0, 21.0);

        Ok(Self {
            socket_path: args
                .socket
                .clone()
                .or(file.socket)
                .map(expand_tilde)
                .unwrap_or_else(|| runtime_dir.join("skeind.sock")),
            template_dir: args
                .template_dir
                .clone()
                .or(file.template_dir)
                .map(expand_tilde)
                .unwrap_or_else(|| config_dir.join("skein/templates")),
            wallpaper_dir: args
                .wallpaper_dir
                .clone()
                .or(file.wallpaper_dir)
                .map(expand_tilde)
                .unwrap_or_else(|| config_dir.join("skein/wallpaper")),
            output_dir: args
                .output_dir
                .clone()
                .or(file.output_dir)
                .map(expand_tilde)
                .unwrap_or_else(|| cache_dir.join("skein")),
            prefer_dark,
            scheme,
            seed,
            contrast,
        })
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

    let args = Args::parse();
    let config = Config::resolve(&args)?;

    // One-shot mode: extract theme from a single image and exit.
    if let Some(image_path) = &args.image {
        let prefer_dark = config.prefer_dark;
        let scheme = config.scheme;
        let contrast = config.contrast;
        let theme = if let Some(seed_hex) = config.seed.clone() {
            tokio::task::spawn_blocking(move || {
                color::theme_from_seed_hex(&seed_hex, prefer_dark, scheme, contrast)
            })
            .await??
        } else {
            let bytes = tokio::fs::read(image_path)
                .await
                .with_context(|| format!("Failed to read {}", image_path.display()))?;
            tokio::task::spawn_blocking(move || {
                color::extract_theme_from_bytes(&bytes, prefer_dark, scheme, contrast)
            })
            .await??
        };

        if args.json {
            println!("{}", serde_json::to_string_pretty(&theme)?);
        } else {
            template::seed_builtin_templates(&config.template_dir).await?;
            let renderer =
                TemplateRenderer::load(&config.template_dir, &config.output_dir).await?;
            renderer.render_all(&theme).await?;
            println!("Rendered templates to {}", config.output_dir.display());
            println!("{}", serde_json::to_string_pretty(&theme)?);
        }
        return Ok(());
    }

    info!("skeind starting");

    let initial_theme = Arc::new(fallback_theme());
    let (theme_tx, theme_rx) = watch::channel(Arc::clone(&initial_theme));

    template::seed_builtin_templates(&config.template_dir).await?;

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

    // A pinned seed replaces the fallback and disables the wallpaper monitor.
    if let Some(seed_hex) = &config.seed {
        let theme = color::theme_from_seed_hex(
            seed_hex,
            config.prefer_dark,
            config.scheme,
            config.contrast,
        )
        .with_context(|| format!("Invalid --seed value: {seed_hex}"))?;
        let arc = Arc::new(theme);
        info!(bg = %arc.background, primary = %arc.primary, "Applying pinned seed theme");
        broadcast_and_render(&arc, &theme_tx, &renderer, &config.output_dir);

        info!("Daemon running with pinned seed theme — wallpaper monitor disabled");
        let _ = signal::ctrl_c().await;
        let _ = std::fs::remove_file(&config.socket_path);
        info!("skeind stopped");
        return Ok(());
    }

    let mut wallpaper_rx = wallpaper::spawn_wallpaper_monitor(&config.wallpaper_dir)
        .await
        .context("Failed to start wallpaper monitor")?;

    info!("All subsystems started — waiting for events");

    let shutdown = signal::ctrl_c();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            biased;

            _ = &mut shutdown => {
                info!("Shutdown signal received");
                break;
            }

            maybe_wall = wallpaper_rx.recv() => {
                match maybe_wall {
                    Some(event) => {
                        info!(path = %event.path.display(), "Wallpaper event triggered");
                        if let Ok(bytes) = tokio::fs::read(&event.path).await {
                            let prefer_dark = config.prefer_dark;
                            let scheme = config.scheme;
                            let contrast = config.contrast;
                            let theme_res = tokio::task::spawn_blocking(move || {
                                color::extract_theme_from_bytes(&bytes, prefer_dark, scheme, contrast)
                            })
                            .await;

                            match theme_res {
                                Ok(Ok(theme)) => {
                                    let arc_theme = Arc::new(theme);
                                    info!(bg = %arc_theme.background, primary = %arc_theme.primary, "Applying wallpaper theme");
                                    broadcast_and_render(&arc_theme, &theme_tx, &renderer, &config.output_dir);
                                }
                                Ok(Err(e)) => warn!(error = %e, "Theme extraction failed"),
                                Err(e) => warn!(error = %e, "Theme extraction task panicked"),
                            }
                        }
                    }
                    None => {
                        warn!("Wallpaper channel closed");
                        break;
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
    output_dir: &std::path::Path,
) {
    let _ = theme_tx.send(Arc::clone(theme));
    let renderer = Arc::clone(renderer);
    let theme_for_tmpl = Arc::clone(theme);
    let snapshot_path = output_dir.join("theme.json");
    tokio::spawn(async move {
        if let Err(e) = renderer.render_all(&theme_for_tmpl).await {
            warn!(error = %e, "Template render pass failed");
        }
        if let Ok(json) = serde_json::to_string_pretty(&*theme_for_tmpl) {
            let tmp = snapshot_path.with_extension("skein.tmp");
            if tokio::fs::write(&tmp, &json).await.is_ok() {
                let _ = tokio::fs::rename(&tmp, &snapshot_path).await;
            }
        }
    });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_tilde() {
        let home = dirs_next::home_dir().unwrap();
        assert_eq!(expand_tilde(PathBuf::from("~")), home);
        assert_eq!(
            expand_tilde(PathBuf::from("~/foo/bar")),
            home.join("foo/bar")
        );
        assert_eq!(
            expand_tilde(PathBuf::from("/abs/path")),
            PathBuf::from("/abs/path")
        );
    }
}
