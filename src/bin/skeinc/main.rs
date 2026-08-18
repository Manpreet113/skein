mod app;
mod events;
mod net;
mod ui;

use std::{
    io::{self, stdout},
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

#[tokio::main]
async fn main() -> Result<()> {
    let (theme_tx, mut theme_rx) = mpsc::channel::<Theme>(8);
    let (socket_tx, socket_rx) = mpsc::channel::<Theme>(8);

    let socket_path = resolve_socket_path();
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
