use std::{path::PathBuf, sync::Arc};
use ratatui::style::Color;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::{mpsc, Mutex},
};

use super::app::{App, Theme};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Connected,
    Disconnected(String),
}

impl ConnState {
    pub fn label(&self) -> String {
        match self {
            Self::Connecting => " Connecting…".into(),
            Self::Connected => " Connected".into(),
            Self::Disconnected(r) => format!(" Disconnected: {r}"),
        }
    }

    pub fn color(&self) -> Color {
        match self {
            Self::Connecting => Color::Yellow,
            Self::Connected => Color::Green,
            Self::Disconnected(_) => Color::Red,
        }
    }
}

pub fn resolve_socket_path() -> PathBuf {
    std::env::var("XDG_RUNTIME_DIR")
        .map(|d| PathBuf::from(d).join("skeind.sock"))
        .unwrap_or_else(|_| PathBuf::from("/tmp/skeind.sock"))
}

pub async fn connect_and_pump(
    path: PathBuf,
    theme_tx: mpsc::Sender<Theme>,
    mut socket_rx: mpsc::Receiver<Theme>,
    app: Arc<Mutex<App>>,
) {
    let stream = match UnixStream::connect(&path).await {
        Ok(s) => s,
        Err(e) => {
            app.lock().await.conn =
                ConnState::Disconnected(format!("{}: {e}", path.display()));
            return;
        }
    };

    app.lock().await.conn = ConnState::Connected;

    let (read_half, mut write_half) = stream.into_split();
    let reader = BufReader::new(read_half);
    let mut lines = reader.lines();

    loop {
        tokio::select! {
            line = lines.next_line() => {
                match line {
                    Ok(Some(raw)) => {
                        if let Ok(theme) = serde_json::from_str::<Theme>(&raw) {
                            let _ = theme_tx.send(theme).await;
                        }
                    }
                    Ok(None) => {
                        app.lock().await.conn =
                            ConnState::Disconnected("daemon closed connection".into());
                        return;
                    }
                    Err(e) => {
                        app.lock().await.conn =
                            ConnState::Disconnected(format!("read error: {e}"));
                        return;
                    }
                }
            }

            maybe_theme = socket_rx.recv() => {
                match maybe_theme {
                    Some(theme) => {
                        let mut json = match serde_json::to_vec(&theme) {
                            Ok(j) => j,
                            Err(_) => continue,
                        };
                        json.push(b'\n');
                        if write_half.write_all(&json).await.is_err() {
                            app.lock().await.conn =
                                ConnState::Disconnected("write error".into());
                            return;
                        }
                    }
                    None => return,
                }
            }
        }
    }
}
