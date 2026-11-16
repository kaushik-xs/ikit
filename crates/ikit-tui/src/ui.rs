//! Drawing. Colors are the terminal's own named colors, so the TUI follows the user's theme.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, Wrap,
};
use serde_json::Value;

use crate::app::{App, Focus, Load, Modal};
use crate::sections::{SECTIONS, Scope, lookup, text};

const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;

pub fn draw(f: &mut Frame, app: &mut App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(5),
        Constraint::Length(2),
    ])
    .areas(f.area());

    draw_header(f, app, header);
    let [side, main] =
        Layout::horizontal([Constraint::Length(20), Constraint::Min(20)]).areas(body);
    draw_sidebar(f, app, side);
    draw_main(f, app, main);
    draw_footer(f, app, footer);

    match &app.modal {
        None => {}
        Some(Modal::Help) => draw_help(f),
        Some(Modal::Input { pending, buffer }) => {
            let (prompt, n, total) = App::input_prompt(pending);
            let title = format!(" {} ({n}/{total}) ", pending.label);
            draw_prompt(
                f,
                &title,
                prompt,
                buffer,
                "Enter: next · Esc: cancel · empty skips optional fields",
            );
        }
        Some(Modal::Palette { buffer, terminal }) => {
            let (title, hint) = if *terminal {
                (
                    " ikit command (in the terminal) ",
                    "Enter: run attached to the terminal, for prompts · Tab: run in a panel · Esc: cancel",
                )
            } else {
                (
                    " ikit command ",
                    "Enter: run in a panel · Tab: run attached to the terminal · Esc: cancel",
                )
            };
            draw_prompt(f, title, "ikit", buffer, hint);
        }
        Some(Modal::Confirm { question, args, .. }) => {
            let area = centered(f.area(), 70, 7);
            f.render_widget(Clear, area);
            let cmd = app.runner.display(args);
            let body = Text::from(vec![
                Line::from(question.as_str()).bold(),
                Line::from(""),
                Line::from(Span::styled(format!("$ {cmd}"), Style::new().fg(MUTED))),
                Line::from(""),
                Line::from(vec![
                    Span::styled(" y ", Style::new().fg(Color::Black).bg(Color::Yellow)),
                    Span::raw(" yes   "),
                    Span::styled(" n ", Style::new().fg(Color::Black).bg(MUTED)),
                    Span::raw(" no"),
                ]),
            ]);
            f.render_widget(
                Paragraph::new(body)
                    .wrap(Wrap { trim: false })
                    .block(modal_block(" Confirm ").border_style(Style::new().fg(Color::Yellow))),
                area,
            );
        }
        Some(Modal::Output {
            title,
            lines,
            done,
            scroll,
            ..
        }) => draw_output(f, title, lines, done.as_ref(), *scroll),
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let sel = |label: &str, v: &Option<String>| {
        vec![
            Span::styled(format!(" {label} "), Style::new().fg(MUTED)),
            match v {
                Some(v) => Span::styled(v.clone(), Style::new().fg(ACCENT).bold()),
                None => Span::styled("none", Style::new().fg(MUTED).italic()),
            },
        ]
    };
    let mut spans = vec![Span::styled(
        " ikit ",
        Style::new().fg(Color::Black).bg(ACCENT).bold(),
    )];
    spans.extend(sel("org", &app.org));
    spans.push(Span::styled("  │", Style::new().fg(MUTED)));
    spans.extend(sel("workload", &app.wl));
    if app.reselecting {
        spans.push(Span::styled("  reading selection…", Style::new().fg(MUTED)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_sidebar(f: &mut Frame, app: &App, area: Rect) {
    let mut last_scope = None;
    let mut items = Vec::new();
    let mut selected = 0;
    for (i, s) in SECTIONS.iter().enumerate() {
        let scope = match s.scope {
            Scope::Home | Scope::Org => "org",
            Scope::Workload => "workload",
        };
        if last_scope != Some(scope) && i > 0 {
            items.push(ListItem::new(Line::styled(
                format!("─ {scope} "),
                Style::new().fg(MUTED),
            )));
        }
        last_scope = Some(scope);
        if i == app.section {
            selected = items.len();
        }
        let num = if i < 9 {
            format!("{} ", i + 1)
        } else {
            "  ".to_string()
        };
        items.push(ListItem::new(Line::from(vec![
            Span::styled(num, Style::new().fg(MUTED)),
            Span::raw(s.title),
        ])));
    }
    let focused = app.focus == Focus::Sidebar;
    let list = List::new(items)
        .block(panel(" ikit ", focused))
        .highlight_style(if focused {
            Style::new().fg(Color::Black).bg(ACCENT).bold()
        } else {
            Style::new()
                .fg(ACCENT)
                .bold()
                .add_modifier(Modifier::REVERSED)
        });
    let mut state = ListState::default().with_selected(Some(selected));
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_main(f: &mut Frame, app: &mut App, area: Rect) {
    let s = app.current();
    let [hint, rest] = Layout::vertical([Constraint::Length(1), Constraint::Min(3)]).areas(area);
    let mut hint_spans = vec![Span::styled(format!(" {}", s.hint), Style::new().fg(MUTED))];
    if app.filtering || !app.filter.is_empty() {
        hint_spans = vec![
            Span::styled(" filter: ", Style::new().fg(Color::Yellow)),
            Span::raw(app.filter.clone()),
            Span::styled(
                if app.filtering { "▏" } else { "" },
                Style::new().fg(Color::Yellow),
            ),
        ];
    }
    f.render_widget(Paragraph::new(Line::from(hint_spans)), hint);

    let wide = rest.width >= 110;
    let [table_area, detail_area] = if wide {
        Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)]).areas(rest)
    } else {
        Layout::vertical([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(rest)
    };

    let focused = app.focus == Focus::Table;
    let title = format!(" {} ", s.title);
    let missing = match s.scope {
        Scope::Home => None,
        Scope::Org if app.org.is_none() => Some("No org selected. Press o to pick or create one."),
        Scope::Workload if app.org.is_none() => {
            Some("No org selected. Press o to pick or create one.")
        }
        Scope::Workload if app.wl.is_none() => {
            Some("No workload selected. Press w to pick or create one.")
        }
        _ => None,
    };
    let message = |msg: &str, style: Style| {
        Paragraph::new(msg.to_string())
            .style(style)
            .wrap(Wrap { trim: false })
            .block(panel(&title, focused))
    };
    if let Some(m) = missing {
        f.render_widget(message(m, Style::new().fg(Color::Yellow)), table_area);
        draw_detail(f, None, app.detail_scroll, detail_area);
        return;
    }
    match &app.data[app.section].load {
        Load::Idle | Load::Loading => {
            f.render_widget(message("Loading…", Style::new().fg(MUTED)), table_area);
            draw_detail(f, None, 0, detail_area);
            return;
        }
        Load::Failed(e) => {
            f.render_widget(message(e, Style::new().fg(Color::Red)), table_area);
            draw_detail(f, None, 0, detail_area);
            return;
        }
        Load::Loaded(_) => {}
    }

    let rows = app.visible_rows();
    if rows.is_empty() {
        let msg = if app.filter.is_empty() {
            format!("Nothing here yet.{}", first_action_hint(app))
        } else {
            "No rows match the filter (Esc clears it).".to_string()
        };
        f.render_widget(message(&msg, Style::new().fg(MUTED)), table_area);
        draw_detail(f, None, 0, detail_area);
        return;
    }

    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            s.columns
                .iter()
                .map(|(_, path)| cell(app, r, path))
                .collect()
        })
        .collect();
    let mut widths: Vec<u16> = s
        .columns
        .iter()
        .map(|(h, _)| h.chars().count() as u16)
        .collect();
    for row in &cells {
        for (i, c) in row.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count().min(48) as u16);
        }
    }
    let constraints: Vec<Constraint> = widths
        .iter()
        .enumerate()
        .map(|(i, w)| {
            if i + 1 == widths.len() {
                Constraint::Min(*w.min(&12))
            } else {
                Constraint::Length(*w)
            }
        })
        .collect();
    let header = Row::new(
        s.columns
            .iter()
            .map(|(h, _)| Cell::from(*h).style(Style::new().fg(MUTED).bold())),
    );
    let body = cells.into_iter().map(|row| {
        Row::new(row.into_iter().map(|c| {
            let style = status_style(&c);
            Cell::from(c).style(style)
        }))
    });
    let count = format!(" {} ", rows.len());
    let detail = app.selected_row().cloned();
    let table = Table::new(body, constraints)
        .header(header)
        .column_spacing(2)
        .row_highlight_style(if focused {
            Style::new().bg(Color::Indexed(236)).fg(ACCENT).bold()
        } else {
            Style::new().add_modifier(Modifier::BOLD)
        })
        .highlight_symbol("▶ ")
        .block(panel(&title, focused).title_bottom(Line::from(count).right_aligned()));
    f.render_stateful_widget(table, table_area, &mut app.data[app.section].table);
    draw_detail(f, detail.as_ref(), app.detail_scroll, detail_area);
}

fn first_action_hint(app: &App) -> String {
    app.current()
        .actions
        .iter()
        .find(|a| !a.needs_selection())
        .map(|a| format!(" Press {} to {}.", key_label(a.key), a.label))
        .unwrap_or_default()
}

fn cell(app: &App, row: &Value, path: &str) -> String {
    if path == "_active" {
        let name = row.get("name").and_then(Value::as_str);
        let current = match app.current().title {
            "Orgs" => app.org.as_deref(),
            _ => app.wl.as_deref(),
        };
        return if name.is_some() && name == current {
            "●".into()
        } else {
            String::new()
        };
    }
    let mut s = text(lookup(row, path));
    if path.ends_with("_at") && s.len() > 16 && s.as_bytes().get(10) == Some(&b'T') {
        s = format!("{} {}", &s[..10], &s[11..16]);
    }
    s
}

fn status_style(c: &str) -> Style {
    match c {
        "ok" | "applied" | "ready" | "running" | "healthy" | "created" | "●" => {
            Style::new().fg(Color::Green)
        }
        "failed" | "error" | "unhealthy" | "exited" | "dead" | "unreachable" => {
            Style::new().fg(Color::Red)
        }
        "draft" | "pending" | "starting" | "restarting" | "destroyed" => {
            Style::new().fg(Color::Yellow)
        }
        _ => Style::new(),
    }
}

fn draw_detail(f: &mut Frame, row: Option<&Value>, scroll: u16, area: Rect) {
    let text = match row {
        None => Text::styled("", Style::new().fg(MUTED)),
        Some(v) => {
            let mut v = v.clone();
            if let Some(m) = v.as_object_mut() {
                m.remove("_key");
            }
            json_text(&v)
        }
    };
    f.render_widget(
        Paragraph::new(text)
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false })
            .block(panel(" Details (J/K scroll) ", false)),
        area,
    );
}

/// Pretty JSON with keys and values told apart.
fn json_text(v: &Value) -> Text<'static> {
    let pretty = serde_json::to_string_pretty(v).unwrap_or_default();
    let lines = pretty.lines().map(|l| {
        let indent_len = l.len() - l.trim_start().len();
        let (indent, rest) = l.split_at(indent_len);
        if let Some(stripped) = rest.strip_prefix('"')
            && let Some(end) = stripped.find("\": ")
        {
            let key = &stripped[..end];
            let value = &stripped[end + 3..];
            return Line::from(vec![
                Span::raw(indent.to_string()),
                Span::styled(key.to_string(), Style::new().fg(ACCENT)),
                Span::styled(": ", Style::new().fg(MUTED)),
                value_span(value),
            ]);
        }
        Line::from(vec![Span::raw(indent.to_string()), value_span(rest)])
    });
    Text::from(lines.collect::<Vec<_>>())
}

fn value_span(v: &str) -> Span<'static> {
    let t = v.trim_end_matches(',');
    let style = if t.starts_with('"') {
        Style::new().fg(Color::Green)
    } else if t == "null" {
        Style::new().fg(MUTED)
    } else if t == "true" || t == "false" || t.parse::<f64>().is_ok() {
        Style::new().fg(Color::Magenta)
    } else {
        Style::new().fg(MUTED)
    };
    Span::styled(v.to_string(), style)
}

fn key_label(c: char) -> String {
    match c {
        '\n' => "⏎".into(),
        c => c.to_string(),
    }
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let [actions, status] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    let key = |k: String| Span::styled(format!(" {k} "), Style::new().fg(Color::Black).bg(ACCENT));
    let mut spans = Vec::new();
    for a in app.current().actions {
        spans.push(key(key_label(a.key)));
        spans.push(Span::raw(format!(" {}  ", a.label)));
    }
    if spans.is_empty() {
        spans.push(Span::styled(" read-only ", Style::new().fg(MUTED)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), actions);

    let line = match &app.status {
        Some((msg, true)) => Line::styled(format!(" {msg}"), Style::new().fg(Color::Red)),
        Some((msg, false)) => Line::styled(format!(" {msg}"), Style::new().fg(Color::Yellow)),
        None => {
            let k = |k: &str| Span::styled(k.to_string(), Style::new().fg(ACCENT).bold());
            let d = |d: &str| Span::styled(d.to_string(), Style::new().fg(MUTED));
            Line::from(vec![
                d(" "),
                k("↑↓"),
                d(" move  "),
                k("Tab"),
                d(" sections  "),
                k("o/w"),
                d(" org/workload  "),
                k("/"),
                d(" filter  "),
                k(":"),
                d(" any ikit command  "),
                k("^R"),
                d(" refresh  "),
                k("?"),
                d(" help  "),
                k("q"),
                d(" quit"),
            ])
        }
    };
    f.render_widget(Paragraph::new(line), status);
}

fn draw_prompt(f: &mut Frame, title: &str, prompt: &str, buffer: &str, hint: &str) {
    let area = centered(f.area(), 76, 6);
    f.render_widget(Clear, area);
    let body = Text::from(vec![
        Line::from(prompt.to_string()).bold(),
        Line::from(vec![
            Span::styled("› ", Style::new().fg(ACCENT)),
            Span::raw(buffer.to_string()),
            Span::styled("▏", Style::new().fg(ACCENT)),
        ]),
        Line::from(""),
        Line::styled(hint.to_string(), Style::new().fg(MUTED)),
    ]);
    f.render_widget(
        Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .block(modal_block(title)),
        area,
    );
}

fn draw_output(
    f: &mut Frame,
    title: &str,
    lines: &[String],
    done: Option<&(String, bool)>,
    scroll: usize,
) {
    let full = f.area();
    let area = Rect {
        x: full.x + 2,
        y: full.y + 1,
        width: full.width.saturating_sub(4),
        height: full.height.saturating_sub(2),
    };
    f.render_widget(Clear, area);
    let inner_h = area.height.saturating_sub(2) as usize;
    let end = lines.len().saturating_sub(scroll);
    let start = end.saturating_sub(inner_h);
    let body: Vec<Line> = lines[start..end]
        .iter()
        .map(|l| Line::raw(l.clone()))
        .collect();
    let (state, style) = match done {
        None => (
            " running… (Esc hides it) ".to_string(),
            Style::new().fg(Color::Yellow),
        ),
        Some((s, true)) => (
            format!(" {s} · Esc to close "),
            Style::new().fg(Color::Green),
        ),
        Some((s, false)) => (format!(" {s} · Esc to close "), Style::new().fg(Color::Red)),
    };
    let block = modal_block(&format!(" $ {title} "))
        .border_style(style)
        .title_bottom(Line::styled(state, style).right_aligned())
        .title_bottom(Line::styled(
            " ↑↓ PgUp PgDn scroll ",
            Style::new().fg(MUTED),
        ));
    f.render_widget(Paragraph::new(body).block(block), area);
}

fn draw_help(f: &mut Frame) {
    let area = centered(f.area(), 84, 30);
    f.render_widget(Clear, area);
    let h = |s: &str| Line::styled(s.to_string(), Style::new().fg(ACCENT).bold());
    let k = |key: &str, what: &str| {
        Line::from(vec![
            Span::styled(format!("  {key:<14}"), Style::new().bold()),
            Span::raw(what.to_string()),
        ])
    };
    let lines = vec![
        h("The flow"),
        Line::raw("  Orgs → Profiles → Registry → Targets → Workloads → Groups/Services"),
        Line::raw("  → Deploy (plan, apply) → Routes, Hooks → Resources, Runs, Backups"),
        Line::raw(""),
        h("Keys"),
        k("↑↓ j k", "move in the focused list"),
        k("Tab ← →", "switch between sections and the table"),
        k("1-9", "jump to a section"),
        k("o / w", "go to Orgs / Workloads (Enter selects one)"),
        k("letters, ⏎", "the section's actions, listed at the bottom"),
        k("/", "filter the rows"),
        k("J / K", "scroll the details"),
        k(":", "run any ikit command; its output shows in a panel"),
        k(
            "!",
            "run any ikit command attached to the terminal (prompts)",
        ),
        k("Ctrl-R, F5", "refresh"),
        k("q, Ctrl-C", "quit"),
        Line::raw(""),
        h("How it works"),
        Line::raw("  Every list and action runs the ikit CLI itself (shown as $ ikit …), with"),
        Line::raw("  the selected org and workload passed as --org and --wl. Remote locks,"),
        Line::raw("  syncing and confirmations behave as on the command line. Commands that"),
        Line::raw("  ask questions (pickers, secrets, typed confirmations) run with the TUI"),
        Line::raw("  suspended, then come back here."),
        Line::raw(""),
        Line::styled("  Esc closes this help.", Style::new().fg(MUTED)),
    ];
    f.render_widget(Paragraph::new(lines).block(modal_block(" Help ")), area);
}

fn panel(title: &str, focused: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(if focused {
            Style::new().fg(ACCENT)
        } else {
            Style::new().fg(MUTED)
        })
        .title(Span::styled(title.to_string(), Style::new().bold()))
}

fn modal_block(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(Span::styled(title.to_string(), Style::new().bold()))
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    let [_, mid, _] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(h),
            Constraint::Fill(1),
        ])
        .areas(area);
    let [_, c, _] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(w),
        Constraint::Fill(1),
    ])
    .areas(mid);
    c
}
