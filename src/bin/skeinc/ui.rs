use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, Padding, Paragraph, Wrap},
    Frame,
};

use super::app::{
    contrasting_text, parse_hex_to_ratatui, ALL_ROLES, App, ColorRole, OklchParam,
};

pub fn render(f: &mut Frame, state: &App) {
    let area = f.area();

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Fill(1), Constraint::Length(1)])
        .split(area);

    let content_area = outer[0];
    let status_area = outer[1];

    let content = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(content_area);

    render_color_matrix(f, content[0], state);
    render_inspector(f, content[1], state);
    render_status_bar(f, status_area, state);
}

fn render_color_matrix(f: &mut Frame, area: Rect, state: &App) {
    let block = Block::default()
        .title(Line::from(" ◈ Palette ").bold())
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .padding(Padding::uniform(1));

    let inner = block.inner(area);
    f.render_widget(block, area);

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
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(8), Constraint::Fill(1)])
        .split(area);

    let swatch_style = Style::default().bg(color);
    let swatch_char = if selected { "▶" } else { " " };
    let swatch_line = Line::from(vec![Span::styled(
        format!(" {swatch_char}    "),
        swatch_style.fg(contrasting_text(color)),
    )]);
    let swatch = Paragraph::new(vec![Line::from(""), swatch_line, Line::from("")])
        .style(swatch_style);
    f.render_widget(swatch, cols[0]);

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

    let label_para = Paragraph::new(vec![Line::from(""), Line::from(vec![role_name, hex_span])]);
    f.render_widget(label_para, label_inner);
}

fn render_inspector(f: &mut Frame, area: Rect, state: &App) {
    let selected_role = state.selected_color_role();
    let active_hex = selected_role.hex(&state.local_theme);
    let active_color = parse_hex_to_ratatui(active_hex);
    let (l, c, h) = super::app::hex_to_oklch(active_hex);

    let block = Block::default()
        .title(Line::from(format!(" ◉ Oklch Inspector — {} ", selected_role.label())).bold())
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .padding(Padding::symmetric(2, 1));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Fill(1),
            Constraint::Length(5),
        ])
        .split(inner);

    let preview_text = format!("  {} — {} ", selected_role.label(), active_hex);
    let preview = Paragraph::new(Line::from(Span::styled(
        preview_text,
        Style::default()
            .bg(active_color)
            .fg(contrasting_text(active_color))
            .bold(),
    )))
    .block(Block::default().padding(Padding::vertical(1)));
    f.render_widget(preview, sections[0]);

    render_oklch_gauge(
        f,
        sections[2],
        "L  Lightness",
        l,
        1.0,
        state.oklch_param == OklchParam::Lightness,
        active_color,
    );
    render_oklch_gauge(
        f,
        sections[3],
        "C  Chroma   ",
        c,
        0.4,
        state.oklch_param == OklchParam::Chroma,
        active_color,
    );
    render_oklch_gauge(
        f,
        sections[4],
        "H  Hue      ",
        h,
        360.0,
        state.oklch_param == OklchParam::Hue,
        active_color,
    );

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
    f.render_widget(Paragraph::new(help).wrap(Wrap { trim: false }), sections[6]);
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

    let bar_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Fill(1), Constraint::Fill(1)])
        .split(area);

    f.render_widget(Paragraph::new(left).style(Style::default().bg(Color::Black)), bar_cols[0]);
    f.render_widget(
        Paragraph::new(right).style(Style::default().bg(Color::Black)),
        bar_cols[1],
    );
}
