use crate::app::{App, Focus, Severity, char_to_byte_idx};
use crate::model::{
    PROMPT_PREVIEW_MAX, iso_time, short_id, shorten_home, truncate_chars, truncate_display_width,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Gauge, Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState,
    Table, Wrap,
};
use unicode_width::UnicodeWidthStr;

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let show_loading = app.loading;
    let toast_height = app.notifications.len().min(2) as u16;

    let mut constraints = vec![
        Constraint::Length(1), // Header
        Constraint::Length(3), // Filter input
    ];
    if show_loading {
        constraints.push(Constraint::Length(2)); // Loading status + gauge
    }
    constraints.push(Constraint::Min(4)); // Sessions table
    constraints.push(Constraint::Length(6)); // Details pane
    if toast_height > 0 {
        constraints.push(Constraint::Length(toast_height)); // Toast notifications
    }
    constraints.push(Constraint::Length(1)); // Footer

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let mut idx = 0;
    render_header(frame, chunks[idx], app);
    idx += 1;

    render_filter(frame, chunks[idx], app);
    idx += 1;

    if show_loading {
        render_loading(frame, chunks[idx], app);
        idx += 1;
    }

    let table_area = chunks[idx];
    app.last_table_height = table_area.height as usize;
    render_table(frame, table_area, app);
    idx += 1;

    render_details(frame, chunks[idx], app);
    idx += 1;

    if toast_height > 0 {
        render_toasts(frame, chunks[idx], app);
        idx += 1;
    }

    render_footer(frame, chunks[idx], app);
}

fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    let title = " OMP Sessions";
    let subtitle = format!("{} ", app.subtitle());
    let total_w = area.width as usize;
    let left_w = title.width();
    let right_w = subtitle.width();
    let gap = total_w.saturating_sub(left_w + right_w);

    let line = Line::from(vec![
        Span::styled(
            title,
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " ".repeat(gap),
            Style::default().fg(Color::Black).bg(Color::Cyan),
        ),
        Span::styled(subtitle, Style::default().fg(Color::Black).bg(Color::Cyan)),
    ]);

    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(Color::Cyan)),
        area,
    );
}

fn render_filter(frame: &mut Frame, area: Rect, app: &App) {
    let focused = app.focus == Focus::Filter && !app.loading;
    let border_style = if app.loading {
        Style::default().fg(Color::DarkGray)
    } else if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::Gray)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Span::styled(
            if focused { " Filter (editing) " } else { " Filter (/) " },
            border_style,
        ));

    let inner = block.inner(area);
    let inner_width = inner.width as usize;

    let (display_line, cursor_x_offset) = if app.filter_query.is_empty() {
        (
            Line::from(Span::styled(
                "Type to filter sessions…",
                Style::default().fg(Color::DarkGray),
            )),
            0u16,
        )
    } else {
        let prefix_bytes = char_to_byte_idx(&app.filter_query, app.filter_cursor);
        let prefix = &app.filter_query[..prefix_bytes];
        let cursor_col = prefix.width();
        let scroll = cursor_col.saturating_sub(inner_width.saturating_sub(1));

        let mut visible = String::new();
        let mut skipped = 0usize;
        let mut shown = 0usize;
        for ch in app.filter_query.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if skipped < scroll {
                skipped += cw;
                continue;
            }
            if shown + cw > inner_width {
                break;
            }
            visible.push(ch);
            shown += cw;
        }
        let x_off = cursor_col.saturating_sub(scroll) as u16;
        let text_style = if app.loading {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::Reset)
        };
        (Line::from(Span::styled(visible, text_style)), x_off)
    };

    frame.render_widget(Paragraph::new(display_line).block(block), area);

    if focused && inner.width > 0 && inner.height > 0 {
        let cx = inner
            .x
            .saturating_add(cursor_x_offset)
            .min(inner.x + inner.width - 1);
        frame.set_cursor_position(Position::new(cx, inner.y));
    }
}

fn render_loading(frame: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(area);

    let p = &app.loading_progress;
    let status = if p.total > 0 && p.stage == "Reading transcripts" {
        format!(" {}… ({}/{})", p.stage, p.done, p.total)
    } else {
        format!(" {}…", p.stage)
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            status,
            Style::default().fg(Color::Gray),
        ))),
        rows[0],
    );

    let ratio = if p.total > 0 {
        (p.done as f64 / p.total as f64).clamp(0.0, 1.0)
    } else {
        let phase = (app.loading_tick % 20) as f64 / 20.0;
        if (app.loading_tick / 20) % 2 == 0 {
            phase
        } else {
            1.0 - phase
        }
    };
    let label = if p.total > 0 {
        format!("{:.0}%", ratio * 100.0)
    } else {
        "working…".to_string()
    };
    let gauge = Gauge::default()
        .gauge_style(Style::default().fg(Color::Cyan).bg(Color::DarkGray))
        .ratio(ratio)
        .label(label);
    frame.render_widget(gauge, rows[1]);
}

fn render_table(frame: &mut Frame, area: Rect, app: &mut App) {
    let has_scrollbar = app.filtered.len() > area.height.saturating_sub(1) as usize;
    let scrollbar_w: usize = usize::from(has_scrollbar && area.width > 1);
    let show_id = area.width >= 120;
    let overhead: usize = (if show_id { 53 } else { 44 }) + scrollbar_w;
    let title_max = (area.width as usize).saturating_sub(overhead).max(10);

    let header_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let mut header_cells = vec![Cell::from(" "), Cell::from("Title")];
    if show_id {
        header_cells.push(Cell::from("ID"));
    }
    header_cells.push(Cell::from("Project"));
    header_cells.push(Cell::from("When"));
    let header = Row::new(header_cells).style(header_style).height(1);

    let rows: Vec<Row> = app
        .filtered
        .iter()
        .map(|s| {
            let mut flag_spans = Vec::new();
            if s.live {
                flag_spans.push(Span::styled(
                    "●",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            if app.selected.contains(&s.id) {
                flag_spans.push(Span::styled(
                    "◆",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            let title_text = truncate_display_width(&s.display_title(), title_max);
            let project_text = truncate_display_width(s.project_name(), 18);
            let when_text = iso_time(s.last_ts);

            let mut cells = vec![
                Cell::from(Line::from(flag_spans)),
                Cell::from(title_text),
            ];
            if show_id {
                cells.push(Cell::from(short_id(&s.id).to_string()));
            }
            cells.push(Cell::from(project_text));
            cells.push(Cell::from(when_text));
            Row::new(cells).height(1)
        })
        .collect();

    let mut widths = vec![Constraint::Length(3), Constraint::Fill(1)];
    if show_id {
        widths.push(Constraint::Length(8));
    }
    widths.push(Constraint::Length(18));
    widths.push(Constraint::Length(19));

    let highlight_style = if app.focus == Focus::Table && !app.loading {
        Style::default()
            .bg(Color::DarkGray)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().bg(Color::DarkGray).fg(Color::Gray)
    };

    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(highlight_style)
        .column_spacing(1);

    let table_render_area = if has_scrollbar && area.width > 1 {
        Rect {
            width: area.width - 1,
            ..area
        }
    } else {
        area
    };

    frame.render_stateful_widget(table, table_render_area, &mut app.table_state);

    if has_scrollbar && area.width > 1 {
        let mut sb_state = ScrollbarState::new(app.filtered.len())
            .position(app.table_state.selected().unwrap_or(0));
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None);
        frame.render_stateful_widget(scrollbar, area, &mut sb_state);
    }
}

fn render_details(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::Cyan));

    let lines = match app.current_session() {
        None => vec![Line::from(Span::styled(
            "No matching sessions",
            Style::default().fg(Color::DarkGray),
        ))],
        Some(s) => {
            let dim = Style::default().fg(Color::DarkGray);
            let val = Style::default().fg(Color::Reset);
            let title_str = if s.title.is_empty() {
                "(untitled)"
            } else {
                s.title.as_str()
            };
            let mut line1 = vec![Span::styled(
                title_str,
                Style::default().add_modifier(Modifier::BOLD),
            )];
            if s.live {
                line1.push(Span::styled(
                    "  ● LIVE",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            let started = iso_time(s.first_ts);
            let project = shorten_home(&s.project, &app.paths.home_dir);
            let prompt = truncate_chars(&s.first_prompt, PROMPT_PREVIEW_MAX).replace('\n', " ");

            vec![
                Line::from(line1),
                Line::from(vec![
                    Span::styled("ID: ", dim),
                    Span::styled(s.id.as_str(), val),
                    Span::styled("  Prompts: ", dim),
                    Span::styled(s.msg_count.to_string(), val),
                    Span::styled("  Started: ", dim),
                    Span::styled(started, val),
                ]),
                Line::from(vec![
                    Span::styled("Project: ", dim),
                    Span::styled(project, val),
                ]),
                Line::from(vec![
                    Span::styled("Prompt: ", dim),
                    Span::styled(prompt, val),
                ]),
            ]
        }
    };

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn render_toasts(frame: &mut Frame, area: Rect, app: &App) {
    let lines: Vec<Line> = app
        .notifications
        .iter()
        .take(2)
        .map(|n| {
            let style = match n.severity {
                Severity::Info => Style::default().fg(Color::Black).bg(Color::Green),
                Severity::Warning => Style::default().fg(Color::Black).bg(Color::Yellow),
                Severity::Error => Style::default().fg(Color::White).bg(Color::Red),
            };
            Line::from(Span::styled(format!(" {} ", n.message), style))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    let key_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Gray)
        .add_modifier(Modifier::BOLD);
    let label_style = Style::default().fg(Color::Gray);

    let bindings: &[(&str, &str)] = match app.focus {
        Focus::Filter => &[("Enter/↓", "Results"), ("Esc", "Clear/Back"), ("^C", "Quit")],
        Focus::Table => &[
            ("q", "Quit"),
            ("/", "Filter"),
            ("Enter", "Resume"),
            ("f", "Restore"),
            ("␣", "Select"),
            ("d", "Delete"),
            ("s", "Sort"),
            ("r", "Refresh"),
        ],
    };

    let mut spans = Vec::with_capacity(bindings.len() * 3);
    for (i, (k, label)) in bindings.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(format!(" {k} "), key_style));
        spans.push(Span::styled(format!(" {label} "), label_style));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}
