// =============================================================================
// skeinc — Interactive TUI Control Center for skeind
// =============================================================================
//
// Architecture:
//
//   ┌───────────────────────────────────────────────────────────────────────┐
//   │  tokio::main                                                          │
//   │                                                                       │
//   │  Channels:                                                            │
//   │    socket_tx  ──→ socket_writer_task  ──→  skeind UDS (write half)   │
//   │    theme_tx   ──→ app state Arc<Mutex<App>>                           │
//   │                                                                       │
//   │  Tasks:                                                               │
//   │    socket_reader_task  reads newline-JSON from skeind → theme_tx     │
//   │    socket_writer_task  serializes outgoing Theme → UDS write half     │
//   │    main event loop     tokio::select!{                                │
//   │                            crossterm EventStream (keys)               │
//   │                            render interval (60 fps)                   │
//   │                            theme_rx (daemon push updates)             │
//   │                        }                                              │
//   └───────────────────────────────────────────────────────────────────────┘
//
// IPC Protocol:
//   skeind  →  skeinc : newline-delimited JSON Theme objects (broadcast)
//   skeinc  →  skeind : newline-delimited JSON Theme objects (patch push)
//
// Keybindings:
//   q / Ctrl+C        — exit
//   h / l             — cycle selected color role left / right
//   j / k or - / +   — nudge selected role's Lightness down / up
//   J / K or [ / ]   — nudge Chroma down / up
//   r                 — reset to daemon's last broadcast
//   Tab               — cycle inspect panel focus (Lightness / Chroma / Hue)
// =============================================================================

use std::{
    io::{self, stdout},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use crossterm::{
    event::{
        Event as CrosstermEvent, EventStream, KeyCode, KeyEvent, KeyModifiers,
        KeyEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{
        Block, Borders, Gauge, Padding, Paragraph, Wrap,
    },
    Frame, Terminal,
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::{mpsc, Mutex},
};

// ---------------------------------------------------------------------------
// Types re-imported from the daemon's color module.
// We duplicate them here so skeinc is a standalone binary with no dep on
// the daemon's internal crate structure.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Theme {
    pub background: String,
    pub surface: String,
    pub surface_variant: String,
    pub primary: String,
    pub accent: String,
    pub text: String,
    pub subtext: String,
    pub seed_oklch: OklchSeed,
    pub dark_mode: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OklchSeed {
    pub lightness: f32,
    pub chroma: f32,
    pub hue: f32,
}

// ---------------------------------------------------------------------------
// Color roles (defines render order in the palette matrix)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorRole {
    Background,
    Surface,
    SurfaceVariant,
    Primary,
    Accent,
    Text,
    Subtext,
}

const ALL_ROLES: [ColorRole; 7] = [
    ColorRole::Background,
    ColorRole::Surface,
    ColorRole::SurfaceVariant,
    ColorRole::Primary,
    ColorRole::Accent,
    ColorRole::Text,
    ColorRole::Subtext,
];

impl ColorRole {
    fn label(self) -> &'static str {
        match self {
            Self::Background => "Background",
            Self::Surface => "Surface",
            Self::SurfaceVariant => "Surface+",
            Self::Primary => "Primary",
            Self::Accent => "Accent",
            Self::Text => "Text",
            Self::Subtext => "Subtext",
        }
    }

    fn hex<'a>(self, t: &'a Theme) -> &'a str {
        match self {
            Self::Background => &t.background,
            Self::Surface => &t.surface,
            Self::SurfaceVariant => &t.surface_variant,
            Self::Primary => &t.primary,
            Self::Accent => &t.accent,
            Self::Text => &t.text,
            Self::Subtext => &t.subtext,
        }
    }

    fn hex_mut<'a>(self, t: &'a mut Theme) -> &'a mut String {
        match self {
            Self::Background => &mut t.background,
            Self::Surface => &mut t.surface,
            Self::SurfaceVariant => &mut t.surface_variant,
            Self::Primary => &mut t.primary,
            Self::Accent => &mut t.accent,
            Self::Text => &mut t.text,
            Self::Subtext => &mut t.subtext,
        }
    }
}

// ---------------------------------------------------------------------------
// Oklch nudge parameter (which channel is `j/k` operating on?)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OklchParam {
    Lightness,
    Chroma,
    Hue,
}

impl OklchParam {
    fn label(self) -> &'static str {
        match self {
            Self::Lightness => "Lightness",
            Self::Chroma => "Chroma",
            Self::Hue => "Hue",
        }
    }

    fn cycle(self) -> Self {
        match self {
            Self::Lightness => Self::Chroma,
            Self::Chroma => Self::Hue,
            Self::Hue => Self::Lightness,
        }
    }
}

// ---------------------------------------------------------------------------
// Connection state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Connected,
    Disconnected(String),
}

impl ConnState {
    fn label(&self) -> String {
        match self {
            Self::Connecting => " Connecting…".into(),
            Self::Connected => " Connected".into(),
            Self::Disconnected(r) => format!(" Disconnected: {r}"),
        }
    }

    fn color(&self) -> Color {
        match self {
            Self::Connecting => Color::Yellow,
            Self::Connected => Color::Green,
            Self::Disconnected(_) => Color::Red,
        }
    }
}

// ---------------------------------------------------------------------------
// Application state
// ---------------------------------------------------------------------------

pub struct App {
    /// The theme as received from the daemon (baseline).
    pub daemon_theme: Theme,
    /// Locally-mutated working copy. May diverge from daemon state.
    pub local_theme: Theme,
    /// Whether `local_theme` differs from `daemon_theme`.
    pub dirty: bool,
    /// Currently selected color role.
    pub selected_role: usize,
    /// Which Oklch parameter `j/k` modulates.
    pub oklch_param: OklchParam,
    /// IPC connection state.
    pub conn: ConnState,
    /// Whether the TUI should exit.
    pub should_quit: bool,
}

impl App {
    fn new(initial_theme: Theme) -> Self {
        Self {
            daemon_theme: initial_theme.clone(),
            local_theme: initial_theme,
            dirty: false,
            selected_role: 0,
            oklch_param: OklchParam::Lightness,
            conn: ConnState::Connecting,
            should_quit: false,
        }
    }

    /// Replace both daemon and local theme (from an incoming daemon push).
    /// Resets dirty state — the daemon is the source of truth.
    fn apply_daemon_theme(&mut self, theme: Theme) {
        self.daemon_theme = theme.clone();
        self.local_theme = theme;
        self.dirty = false;
    }

    /// Revert the local working copy to the last daemon broadcast.
    fn reset_to_daemon(&mut self) {
        self.local_theme = self.daemon_theme.clone();
        self.dirty = false;
    }

    fn selected_color_role(&self) -> ColorRole {
        ALL_ROLES[self.selected_role]
    }

    /// Nudge the seed's Oklch `param` by `delta`, then recompute the selected
    /// role's hex color from the nudged Oklch values.
    fn nudge(&mut self, delta: f32) {
        match self.oklch_param {
            OklchParam::Lightness => {
                self.local_theme.seed_oklch.lightness =
                    (self.local_theme.seed_oklch.lightness + delta).clamp(0.0, 1.0);
            }
            OklchParam::Chroma => {
                self.local_theme.seed_oklch.chroma =
                    (self.local_theme.seed_oklch.chroma + delta).clamp(0.0, 0.4);
            }
            OklchParam::Hue => {
                self.local_theme.seed_oklch.hue =
                    (self.local_theme.seed_oklch.hue + delta).rem_euclid(360.0);
            }
        }

        // Recompute the selected role's hex from the updated Oklch seed.
        let role = self.selected_color_role();
        let hex = oklch_to_hex(
            self.local_theme.seed_oklch.lightness,
            self.local_theme.seed_oklch.chroma,
            self.local_theme.seed_oklch.hue,
        );
        *role.hex_mut(&mut self.local_theme) = hex;

        self.dirty = self.local_theme != self.daemon_theme;
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // Channel: daemon → TUI (incoming theme updates)
    let (theme_tx, mut theme_rx) = mpsc::channel::<Theme>(8);

    // Channel: TUI → socket writer (outgoing theme patches)
    let (socket_tx, socket_rx) = mpsc::channel::<Theme>(8);

    // Resolve socket path.
    let socket_path = resolve_socket_path();

    // Shared app state.
    let initial = fallback_theme();
    let app = Arc::new(Mutex::new(App::new(initial)));

    // Attempt UDS connection (non-blocking; errors update conn state).
    let app_clone = Arc::clone(&app);
    tokio::spawn(async move {
        connect_and_pump(socket_path, theme_tx, socket_rx, app_clone).await;
    });

    // Set up terminal.
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;

    let result = run_event_loop(&mut terminal, app, socket_tx, &mut theme_rx).await;

    // Always restore terminal on exit.
    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    result
}

// ---------------------------------------------------------------------------
// UDS connection + pump
// ---------------------------------------------------------------------------

/// Connect to the daemon socket and run two sub-tasks:
///   - reader: JSON lines from daemon → `theme_tx`
///   - writer: outgoing Theme patches from `socket_rx` → socket
///
/// If connection fails, sets `ConnState::Disconnected` and returns.
async fn connect_and_pump(
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
            // Daemon → TUI: parse newline-delimited JSON Theme.
            line = lines.next_line() => {
                match line {
                    Ok(Some(raw)) => {
                        if let Ok(theme) = serde_json::from_str::<Theme>(&raw) {
                            let _ = theme_tx.send(theme).await;
                        }
                    }
                    Ok(None) => {
                        // Daemon closed the connection.
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

            // TUI → Daemon: serialize and write a patched Theme.
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
                    None => return, // TUI exited.
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Main event loop
// ---------------------------------------------------------------------------

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: Arc<Mutex<App>>,
    socket_tx: mpsc::Sender<Theme>,
    theme_rx: &mut mpsc::Receiver<Theme>,
) -> Result<()> {
    let mut event_stream = EventStream::new();
    // Render at 60 fps; we don't need to re-render unless something changed,
    // but capping at 60 fps ensures < 2 ms of visual latency on any event.
    let mut render_interval = tokio::time::interval(Duration::from_millis(16));
    render_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        // Check quit flag without holding the lock across the select.
        if app.lock().await.should_quit {
            break;
        }

        tokio::select! {
            // ── Render tick ────────────────────────────────────────────────
            _ = render_interval.tick() => {
                let state = app.lock().await;
                terminal.draw(|f| render(f, &state))?;
            }

            // ── Keyboard input ─────────────────────────────────────────────
            maybe_ev = event_stream.next() => {
                let Some(Ok(CrosstermEvent::Key(key))) = maybe_ev else {
                    continue;
                };
                // Only act on key presses (crossterm fires Release on some platforms).
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                let push = handle_key(key, Arc::clone(&app)).await;
                if let Some(theme) = push {
                    let _ = socket_tx.send(theme).await;
                }
            }

            // ── Daemon push (incoming theme update) ────────────────────────
            Some(theme) = theme_rx.recv() => {
                app.lock().await.apply_daemon_theme(theme);
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
/// Key handler — returns `Some(Theme)` if a patch should be sent to daemon
// ---------------------------------------------------------------------------

async fn handle_key(
    key: KeyEvent,
    app: Arc<Mutex<App>>,
) -> Option<Theme> {
    let mut state = app.lock().await;

    match key.code {
        // ── Quit ──────────────────────────────────────────────────────────
        KeyCode::Char('q') => {
            state.should_quit = true;
            return None;
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.should_quit = true;
            return None;
        }

        // ── Cycle color role selection ─────────────────────────────────────
        KeyCode::Char('h') | KeyCode::Left => {
            state.selected_role =
                state.selected_role.saturating_sub(1);
        }
        KeyCode::Char('l') | KeyCode::Right => {
            state.selected_role =
                (state.selected_role + 1).min(ALL_ROLES.len() - 1);
        }

        // ── Nudge Lightness / Chroma / Hue ────────────────────────────────
        // j / k  →  Lightness (or active param)  down / up
        KeyCode::Char('j') | KeyCode::Char('-') => {
            let delta = nudge_step(&state.oklch_param);
            state.nudge(-delta);
        }
        KeyCode::Char('k') | KeyCode::Char('+') => {
            let delta = nudge_step(&state.oklch_param);
            state.nudge(delta);
        }

        // J / K  →  Chroma  down / up (shift modifier)
        KeyCode::Char('J') => {
            state.oklch_param = OklchParam::Chroma;
            state.nudge(-0.01);
        }
        KeyCode::Char('K') => {
            state.oklch_param = OklchParam::Chroma;
            state.nudge(0.01);
        }

        // [ / ] → Hue down / up
        KeyCode::Char('[') => {
            state.oklch_param = OklchParam::Hue;
            state.nudge(-2.0);
        }
        KeyCode::Char(']') => {
            state.oklch_param = OklchParam::Hue;
            state.nudge(2.0);
        }

        // ── Tab: cycle active Oklch parameter ────────────────────────────
        KeyCode::Tab => {
            state.oklch_param = state.oklch_param.cycle();
        }

        // ── r: reset to daemon state ──────────────────────────────────────
        KeyCode::Char('r') => {
            state.reset_to_daemon();
        }

        // ── Enter / Space: push local state to daemon ─────────────────────
        KeyCode::Enter | KeyCode::Char(' ') => {
            if state.dirty {
                return Some(state.local_theme.clone());
            }
        }

        _ => {}
    }

    // Auto-push on every nudge so hot-reload is instantaneous.
    if state.dirty {
        Some(state.local_theme.clone())
    } else {
        None
    }
}

fn nudge_step(param: &OklchParam) -> f32 {
    match param {
        OklchParam::Lightness => 0.01,
        OklchParam::Chroma => 0.005,
        OklchParam::Hue => 1.0,
    }
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

fn render(f: &mut Frame, state: &App) {
    let area = f.area();

    // ── Outer layout: [color matrix | right panel] / status bar ──────────
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Fill(1),   // main content
            Constraint::Length(1), // status bar
        ])
        .split(area);

    let content_area = outer[0];
    let status_area = outer[1];

    // ── Content: left (color matrix) / right (inspector) ─────────────────
    let content = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(55), // color matrix
            Constraint::Percentage(45), // Oklch inspector
        ])
        .split(content_area);

    render_color_matrix(f, content[0], state);
    render_inspector(f, content[1], state);
    render_status_bar(f, status_area, state);
}

// ---------------------------------------------------------------------------
// Color Matrix
// ---------------------------------------------------------------------------

fn render_color_matrix(f: &mut Frame, area: Rect, state: &App) {
    let block = Block::default()
        .title(Line::from(" ◈ Palette ").bold())
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .padding(Padding::uniform(1));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Divide inner area into rows, one per color role.
    let row_height = 3u16;
    let constraints: Vec<Constraint> = ALL_ROLES
        .iter()
        .map(|_| Constraint::Length(row_height))
        .collect();

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    for (i, role) in ALL_ROLES.iter().enumerate() {
        let is_selected = i == state.selected_role;
        let hex = role.hex(&state.local_theme);
        let color = parse_hex_to_ratatui(hex);

        render_color_swatch(f, rows[i], *role, hex, color, is_selected);
    }
}

fn render_color_swatch(
    f: &mut Frame,
    area: Rect,
    role: ColorRole,
    hex: &str,
    color: Color,
    selected: bool,
) {
    // Split row into: swatch block (left) | label + hex (right)
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(8),  // color swatch
            Constraint::Fill(1),    // label
        ])
        .split(area);

    // ── Swatch: filled rectangle in the role's color ───────────────────
    let swatch_style = Style::default().bg(color);
    // Use the inverse (text) color for the inner character so it's readable.
    let swatch_char = if selected { "▶" } else { " " };
    let swatch_line = Line::from(vec![
        Span::styled(
            format!(" {swatch_char}    "),
            swatch_style.fg(contrasting_text(color)),
        ),
    ]);
    let swatch = Paragraph::new(vec![
        Line::from(""),
        swatch_line,
        Line::from(""),
    ])
    .style(swatch_style);
    f.render_widget(swatch, cols[0]);

    // ── Label block ────────────────────────────────────────────────────
    let border_style = if selected {
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let label_block = Block::default()
        .borders(Borders::LEFT)
        .border_style(border_style)
        .padding(Padding::horizontal(1));

    let label_inner = label_block.inner(cols[1]);
    f.render_widget(label_block, cols[1]);

    let role_name = if selected {
        Span::styled(role.label(), Style::default().fg(color).bold())
    } else {
        Span::styled(role.label(), Style::default().fg(Color::White))
    };

    let hex_span = Span::styled(
        format!("  {hex}"),
        Style::default().fg(color).add_modifier(Modifier::DIM),
    );

    let label_para = Paragraph::new(vec![
        Line::from(""),
        Line::from(vec![role_name, hex_span]),
    ]);
    f.render_widget(label_para, label_inner);
}

// ---------------------------------------------------------------------------
// Oklch Inspector
// ---------------------------------------------------------------------------

fn render_inspector(f: &mut Frame, area: Rect, state: &App) {
    let seed = &state.local_theme.seed_oklch;
    let selected_role = state.selected_color_role();
    let active_hex = selected_role.hex(&state.local_theme);
    let active_color = parse_hex_to_ratatui(active_hex);

    let block = Block::default()
        .title(Line::from(
            format!(" ◉ Oklch Seed — {} ", selected_role.label())
        ).bold())
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .padding(Padding::symmetric(2, 1));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Layout: preview swatch + three gauges + help text
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // preview swatch
            Constraint::Length(1),  // spacer
            Constraint::Length(3),  // L gauge
            Constraint::Length(3),  // C gauge
            Constraint::Length(3),  // H gauge
            Constraint::Fill(1),    // spacer
            Constraint::Length(5),  // controls help
        ])
        .split(inner);

    // ── Full-width color preview ────────────────────────────────────────
    let preview_text = format!("  {} — {} ", selected_role.label(), active_hex);
    let preview = Paragraph::new(
        Line::from(Span::styled(
            preview_text,
            Style::default()
                .bg(active_color)
                .fg(contrasting_text(active_color))
                .bold(),
        ))
    )
    .block(Block::default().padding(Padding::vertical(1)));
    f.render_widget(preview, sections[0]);

    // ── Oklch gauges ───────────────────────────────────────────────────
    render_oklch_gauge(
        f,
        sections[2],
        "L  Lightness",
        seed.lightness,
        1.0,
        state.oklch_param == OklchParam::Lightness,
        active_color,
    );
    render_oklch_gauge(
        f,
        sections[3],
        "C  Chroma   ",
        seed.chroma,
        0.4,
        state.oklch_param == OklchParam::Chroma,
        active_color,
    );
    render_oklch_gauge(
        f,
        sections[4],
        "H  Hue      ",
        seed.hue,
        360.0,
        state.oklch_param == OklchParam::Hue,
        active_color,
    );

    // ── Controls legend ────────────────────────────────────────────────
    let dirty_indicator = if state.dirty {
        Span::styled("  ● Modified", Style::default().fg(Color::Yellow).bold())
    } else {
        Span::styled("  ○ Synced  ", Style::default().fg(Color::Green))
    };

    let help = vec![
        Line::from(dirty_indicator),
        Line::from(""),
        Line::from(vec![
            Span::styled("Tab", Style::default().fg(Color::Cyan).bold()),
            Span::raw(" cycle param  "),
            Span::styled("j/k", Style::default().fg(Color::Cyan).bold()),
            Span::raw(" nudge  "),
        ]),
        Line::from(vec![
            Span::styled("h/l", Style::default().fg(Color::Cyan).bold()),
            Span::raw(" select role  "),
            Span::styled("r", Style::default().fg(Color::Cyan).bold()),
            Span::raw(" reset  "),
        ]),
        Line::from(vec![
            Span::styled("[/]", Style::default().fg(Color::Cyan).bold()),
            Span::raw(" hue  "),
            Span::styled("J/K", Style::default().fg(Color::Cyan).bold()),
            Span::raw(" chroma  "),
            Span::styled("q", Style::default().fg(Color::Red).bold()),
            Span::raw(" quit"),
        ]),
    ];
    f.render_widget(
        Paragraph::new(help).wrap(Wrap { trim: false }),
        sections[6],
    );
}

fn render_oklch_gauge(
    f: &mut Frame,
    area: Rect,
    label: &str,
    value: f32,
    max: f32,
    active: bool,
    accent: Color,
) {
    let ratio = (value / max).clamp(0.0, 1.0) as f64;

    // Format the numeric readout with appropriate precision.
    let display_val = if max > 10.0 {
        format!("{value:6.1}°")
    } else {
        format!("{value:.4}")
    };

    let label_style = if active {
        Style::default().fg(accent).bold()
    } else {
        Style::default().fg(Color::Gray)
    };

    let gauge_style = if active {
        Style::default().fg(accent).bg(Color::Black)
    } else {
        Style::default().fg(Color::DarkGray).bg(Color::Black)
    };

    let active_marker = if active { "▶ " } else { "  " };

    let gauge = Gauge::default()
        .block(
            Block::default()
                .title(Line::from(vec![
                    Span::styled(active_marker, label_style),
                    Span::styled(label, label_style),
                    Span::styled(
                        format!("  {display_val}"),
                        Style::default().fg(Color::White).bold(),
                    ),
                ]))
                .borders(Borders::NONE),
        )
        .gauge_style(gauge_style)
        .ratio(ratio)
        .use_unicode(true);

    f.render_widget(gauge, area);
}

// ---------------------------------------------------------------------------
// Status bar
// ---------------------------------------------------------------------------

fn render_status_bar(f: &mut Frame, area: Rect, state: &App) {
    let conn_style = Style::default().fg(state.conn.color()).bold();
    let conn_label = state.conn.label();

    let dark_mode_label = if state.local_theme.dark_mode {
        "  Dark"
    } else {
        "  Light"
    };

    let left = Line::from(vec![
        Span::styled("  skeinc", Style::default().fg(Color::Magenta).bold()),
        Span::raw("  │"),
        Span::styled(conn_label, conn_style),
        Span::raw("  │"),
        Span::styled(dark_mode_label, Style::default().fg(Color::Blue)),
    ]);

    let right_text = format!(
        "param: {}  role: {} {}  ",
        state.oklch_param.label(),
        state.selected_color_role().label(),
        state.selected_color_role().hex(&state.local_theme),
    );

    let right = Line::from(Span::styled(
        right_text,
        Style::default().fg(Color::DarkGray),
    ))
    .right_aligned();

    let status_para = Paragraph::new(left)
        .style(Style::default().bg(Color::Black));

    // Render left and right side of status bar separately.
    let bar_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Fill(1), Constraint::Fill(1)])
        .split(area);

    f.render_widget(status_para, bar_cols[0]);
    f.render_widget(
        Paragraph::new(right).style(Style::default().bg(Color::Black)),
        bar_cols[1],
    );
}

// ---------------------------------------------------------------------------
// Color utilities
// ---------------------------------------------------------------------------

/// Parse a `#RRGGBB` hex string into a ratatui `Color::Rgb`.
/// Returns `Color::Reset` on malformed input (graceful degradation).
fn parse_hex_to_ratatui(hex: &str) -> Color {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 {
        return Color::Reset;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(128);
    let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(128);
    let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(128);
    Color::Rgb(r, g, b)
}

/// Returns `Color::White` or `Color::Black` for maximum contrast on a
/// background, computed via WCAG relative luminance without pulling palette.
fn contrasting_text(bg: Color) -> Color {
    let (r, g, b) = match bg {
        Color::Rgb(r, g, b) => (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0),
        _ => return Color::White,
    };

    // Linearise sRGB.
    let lin = |c: f32| -> f32 {
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };

    let lum = 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);

    // WCAG: use white text if luminance < 0.179 (contrast ≥ 4.5 on black bg).
    if lum < 0.179 {
        Color::White
    } else {
        Color::Black
    }
}

/// Minimal Oklch → sRGB → #RRGGBB conversion, used to recompute hex values
/// when the user nudges the Oklch seed.
///
/// Implements the exact Björn Ottosson Oklab spec:
///   Oklch → Oklab → linear sRGB → gamma sRGB
fn oklch_to_hex(l: f32, c: f32, h_deg: f32) -> String {
    let h = h_deg.to_radians();
    // Oklch → Oklab
    let a = c * h.cos();
    let b = c * h.sin();

    // Oklab → linear sRGB (Ottosson's matrix)
    let l_ = l + 0.3963377774 * a + 0.2158037573 * b;
    let m_ = l - 0.1055613458 * a - 0.0638541728 * b;
    let s_ = l - 0.0894841775 * a - 1.2914855480 * b;

    let l3 = l_ * l_ * l_;
    let m3 = m_ * m_ * m_;
    let s3 = s_ * s_ * s_;

    let r_lin =  4.0767416621 * l3 - 3.3077115913 * m3 + 0.2309699292 * s3;
    let g_lin = -1.2684380046 * l3 + 2.6097574011 * m3 - 0.3413193965 * s3;
    let b_lin = -0.0041960863 * l3 - 0.7034186147 * m3 + 1.7076147010 * s3;

    // Linear → gamma sRGB
    let gamma = |x: f32| -> f32 {
        let x = x.clamp(0.0, 1.0);
        if x <= 0.0031308 {
            12.92 * x
        } else {
            1.055 * x.powf(1.0 / 2.4) - 0.055
        }
    };

    let r = (gamma(r_lin) * 255.0).round() as u8;
    let g = (gamma(g_lin) * 255.0).round() as u8;
    let b_byte = (gamma(b_lin) * 255.0).round() as u8;

    format!("#{r:02X}{g:02X}{b_byte:02X}")
}

// ---------------------------------------------------------------------------
// Misc helpers
// ---------------------------------------------------------------------------

fn resolve_socket_path() -> PathBuf {
    std::env::var("XDG_RUNTIME_DIR")
        .map(|d| PathBuf::from(d).join("skeind.sock"))
        .unwrap_or_else(|_| PathBuf::from("/tmp/skeind.sock"))
}

/// Fallback theme shown before the daemon responds.
/// Mirrors the daemon's own `fallback_theme()` so there's no visual flash.
fn fallback_theme() -> Theme {
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
            chroma:    0.14,
            hue:       297.0,
        },
        dark_mode: true,
    }
}
