mod app;
mod events;
mod net;
mod ui;

use std::{
    io::{self, stdout},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use crossterm::{
    event::{Event as CrosstermEvent, EventStream, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::{
    sync::{mpsc, Mutex},
    time::MissedTickBehavior,
};

use app::{fallback_theme, App, Theme};
use events::handle_key;
use net::{connect_and_pump, resolve_socket_path};
use ui::render;

#[derive(clap::Parser)]
#[command(name = "skeinc", version, about = "skein TUI client / theme tool")]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Unix socket path of the running daemon (default: $XDG_RUNTIME_DIR/skeind.sock).
    #[arg(long)]
    socket: Option<PathBuf>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Extract a theme from an image, render all templates to a preview dir,
    /// and print the resulting theme (no daemon needed).
    Preview {
        image: PathBuf,
        #[arg(long, value_enum, default_value_t = Mode::Dark)]
        mode: Mode,
        #[arg(long, value_enum, default_value_t = skein::color::Scheme::Tonal)]
        scheme: skein::color::Scheme,
        #[arg(long)]
        seed: Option<String>,
        #[arg(long, default_value_t = skein::color::WCAG_AA_NORMAL)]
        contrast: f32,
    },
    /// Extract a theme from an image and print JSON (no daemon needed).
    Generate {
        image: PathBuf,
        #[arg(long, value_enum, default_value_t = Mode::Dark)]
        mode: Mode,
        #[arg(long, value_enum, default_value_t = skein::color::Scheme::Tonal)]
        scheme: skein::color::Scheme,
        /// Explicit seed color ("#RRGGBB") instead of extracting from the image.
        #[arg(long)]
        seed: Option<String>,
        /// Minimum text contrast ratio against the background.
        #[arg(long, default_value_t = skein::color::WCAG_AA_NORMAL)]
        contrast: f32,
    },
    /// Tell the running daemon to re-extract the theme from an explicit
    /// image path (the handoff for wallpaper pickers), then print the
    /// resulting theme.
    Set {
        path: PathBuf,
    },
    /// Apply `[deploy]` placement from config.toml right now (symlink
    /// rendered outputs into live locations) without needing a theme
    /// change or daemon restart.
    Deploy {
        /// Path to config file (default: ~/.config/skein/config.toml).
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Mode {
    Dark,
    Light,
    Auto,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = <Args as clap::Parser>::parse();

    if let Some(Command::Preview {
        image,
        mode,
        scheme,
        seed,
        contrast,
    }) = &args.command
    {
        let prefer_dark = match mode {
            Mode::Dark => true,
            Mode::Light => false,
            Mode::Auto => skein::color::system_prefers_dark(),
        };
        let scheme = *scheme;
        let contrast = *contrast;
        let theme = if let Some(seed_hex) = seed {
            let seed_hex = seed_hex.clone();
            tokio::task::spawn_blocking(move || {
                skein::color::theme_from_seed_hex(&seed_hex, prefer_dark, scheme, contrast)
            })
            .await??
        } else {
            let bytes = tokio::fs::read(&image)
                .await
                .with_context(|| format!("Failed to read {}", image.display()))?;
            tokio::task::spawn_blocking(move || {
                skein::color::extract_theme_from_bytes(&bytes, prefer_dark, scheme, contrast)
            })
            .await??
        };

        let cfg_dir = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                dirs_next::config_dir().unwrap_or_else(|| PathBuf::from("~/.config"))
            });
        let template_dir = cfg_dir.join("skein/templates");
        let preview_dir = std::env::var("XDG_CACHE_HOME")
            .map(|d| PathBuf::from(d).join("skein/preview"))
            .unwrap_or_else(|_| {
                dirs_next::cache_dir()
                    .unwrap_or_else(|| PathBuf::from("~/.cache"))
                    .join("skein/preview")
            });

        skein::template::seed_builtin_templates(&template_dir).await?;
        let renderer =
            skein::template::TemplateRenderer::load(&template_dir, &preview_dir, None).await?;
        renderer.render_all(&theme).await?;

        println!("Preview rendered to {}", preview_dir.display());
        println!("{}", serde_json::to_string_pretty(&theme)?);
        return Ok(());
    }

    if let Some(Command::Set { path }) = &args.command {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let path = std::fs::canonicalize(path)
            .with_context(|| format!("Cannot read {}", path.display()))?;
        let socket_path = args
            .socket
            .clone()
            .unwrap_or_else(resolve_socket_path);
        let stream = tokio::net::UnixStream::connect(&socket_path)
            .await
            .with_context(|| format!("Cannot connect to {}", socket_path.display()))?;
        let (read_half, mut write_half) = stream.into_split();
        let mut lines = tokio::io::BufReader::new(read_half).lines();

        // Discard the cached-theme line the server sends on connect.
        lines.next_line().await?;

        let cmd = serde_json::json!({"cmd": "set-wallpaper", "path": path}).to_string();
        write_half.write_all(cmd.as_bytes()).await?;
        write_half.write_all(b"\n").await?;
        write_half.flush().await?;

        // Wait for the re-extracted theme broadcast.
        let updated: Theme = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match lines.next_line().await? {
                    Some(line) if !line.trim().is_empty() => {
                        if let Ok(theme) = serde_json::from_str::<Theme>(&line) {
                            return Ok::<_, anyhow::Error>(theme);
                        }
                    }
                    Some(_) => continue,
                    None => anyhow::bail!("daemon closed connection"),
                }
            }
        })
        .await
        .context("Timed out waiting for daemon to apply the wallpaper")??;
        println!("{}", serde_json::to_string_pretty(&updated)?);
        return Ok(());
    }

    if let Some(Command::Deploy { config }) = &args.command {
        #[derive(Debug, Default, serde::Deserialize)]
        struct DeployFile {
            output_dir: Option<PathBuf>,
            #[serde(default)]
            deploy: skein::deploy::DeployFileConfig,
        }

        let config_path = config.clone().unwrap_or_else(|| {
            std::env::var("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| {
                    dirs_next::config_dir().unwrap_or_else(|| PathBuf::from("~/.config"))
                })
                .join("skein/config.toml")
        });
        let file: DeployFile = match std::fs::read_to_string(&config_path) {
            Ok(raw) => toml::from_str(&raw)
                .with_context(|| format!("Failed to parse {}", config_path.display()))?,
            Err(_) => DeployFile::default(),
        };
        let output_dir = file
            .output_dir
            .map(skein::deploy::expand_tilde)
            .unwrap_or_else(|| {
                std::env::var("XDG_CACHE_HOME")
                    .map(|d| PathBuf::from(d).join("skein"))
                    .unwrap_or_else(|_| skein::deploy::expand_tilde("~/.cache/skein".into()))
            });
        let plan = skein::deploy::DeployPlan::resolve(&file.deploy);
        if !plan.enabled() {
            println!("deploy is not enabled (set [deploy] enable = true with targets in config.toml)");
            return Ok(());
        }

        let report = skein::deploy::ensure_deploy(&output_dir, &plan).await;
        println!(
            "deploy: {} linked, {} skipped, {} backed up",
            report.linked, report.skipped, report.backed_up
        );
        return Ok(());
    }

    if let Some(Command::Generate {
        image,
        mode,
        scheme,
        seed,
        contrast,
    }) = args.command
    {
        let prefer_dark = match mode {
            Mode::Dark => true,
            Mode::Light => false,
            Mode::Auto => skein::color::system_prefers_dark(),
        };
        let theme = if let Some(seed_hex) = seed {
            tokio::task::spawn_blocking(move || {
                skein::color::theme_from_seed_hex(&seed_hex, prefer_dark, scheme, contrast)
            })
            .await??
        } else {
            let bytes = tokio::fs::read(&image)
                .await
                .with_context(|| format!("Failed to read {}", image.display()))?;
            tokio::task::spawn_blocking(move || {
                skein::color::extract_theme_from_bytes(&bytes, prefer_dark, scheme, contrast)
            })
            .await??
        };
        println!("{}", serde_json::to_string_pretty(&theme)?);
        return Ok(());
    }

    let (theme_tx, mut theme_rx) = mpsc::channel::<Theme>(8);
    let (socket_tx, socket_rx) = mpsc::channel::<Theme>(8);

    let socket_path = args.socket.clone().unwrap_or_else(resolve_socket_path);
    let initial = fallback_theme();
    let app = Arc::new(Mutex::new(App::new(initial)));

    let app_clone = Arc::clone(&app);
    tokio::spawn(async move {
        connect_and_pump(socket_path, theme_tx, socket_rx, app_clone).await;
    });

    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;

    let result = run_event_loop(&mut terminal, app, socket_tx, &mut theme_rx).await;

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    result
}

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: Arc<Mutex<App>>,
    socket_tx: mpsc::Sender<Theme>,
    theme_rx: &mut mpsc::Receiver<Theme>,
) -> Result<()> {
    let mut event_stream = EventStream::new();
    let mut render_interval = tokio::time::interval(Duration::from_millis(16));
    render_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        if app.lock().await.should_quit {
            break;
        }

        tokio::select! {
            _ = render_interval.tick() => {
                let state = app.lock().await;
                terminal.draw(|f| render(f, &state))?;
            }

            maybe_ev = event_stream.next() => {
                let Some(Ok(CrosstermEvent::Key(key))) = maybe_ev else {
                    continue;
                };
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                let push = handle_key(key, Arc::clone(&app)).await;
                if let Some(theme) = push {
                    let _ = socket_tx.send(theme).await;
                }
            }

            Some(theme) = theme_rx.recv() => {
                app.lock().await.apply_daemon_theme(theme);
            }
        }
    }

    Ok(())
}
