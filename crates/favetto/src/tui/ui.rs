//! ratatui rendering: header tabs, per-tab content, and a connection status bar.

use chrono::{DateTime, Utc};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table, TableState, Tabs, Wrap,
};
use ratatui::Frame;

use favetto_core::model::{MessageRole, TaskStatus};

use super::app::{App, ClickAction, ClickRegion, ConnState, Form, Popup, Tab, MENU_OPTIONS};

pub fn draw(frame: &mut Frame, app: &mut App) {
    app.click_regions.clear();

    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(frame.area());

    // Record tab click regions. The `Tabs` widget renders each title as
    // `" " + title + " "` separated by the default divider `│`, i.e. a leading
    // space then, between titles, a 3-char `" │ "` gap.
    for (tab, col_start, col_end) in tab_click_regions(chunks[0].x, chunks[0].y) {
        app.click_regions.push(ClickRegion {
            row: chunks[0].y,
            col_start,
            col_end,
            action: ClickAction::Tab(tab),
        });
    }

    let titles: Vec<Line> = Tab::ALL
        .iter()
        .map(|t| Line::from(Span::raw(t.label())))
        .collect();
    let tabs = Tabs::new(titles)
        .select(Tab::ALL.iter().position(|t| *t == app.tab).unwrap_or(0))
        .block(Block::default().borders(Borders::BOTTOM))
        .highlight_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
    frame.render_widget(tabs, chunks[0]);

    let tab = app.tab;
    match tab {
        Tab::Tasks => draw_tasks(frame, app, chunks[1]),
        Tab::Catalog => draw_catalog(frame, app, chunks[1]),
        Tab::Chat => draw_chat(frame, app, chunks[1]),
        Tab::Events => draw_events(frame, app, chunks[1]),
        Tab::Scheduler => draw_schedules(frame, app, chunks[1]),
        Tab::Notifications => draw_notifications(frame, app, chunks[1]),
        _ => draw_placeholder(frame, app, chunks[1]),
    }

    draw_status(frame, app, chunks[2]);

    if !matches!(app.popup, Popup::None) {
        draw_popup(frame, app);
    }
}

fn draw_popup(frame: &mut Frame, app: &App) {
    match &app.popup {
        Popup::None => {}
        Popup::Menu { selected } => draw_menu(frame, *selected),
        Popup::Form(form) => draw_form(frame, form),
    }
}

/// A horizontally-centered rectangle of the given width (percent) and fixed height.
fn centered_rect(area: Rect, width_pct: u16, height: u16) -> Rect {
    let w = (area.width.saturating_mul(width_pct) / 100)
        .min(area.width.saturating_sub(4))
        .max(10);
    let h = height.min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

fn draw_menu(frame: &mut Frame, selected: usize) {
    let area = frame.area();
    let rect = centered_rect(area, 60, MENU_OPTIONS.len() as u16 + 2);

    let items: Vec<ListItem> = MENU_OPTIONS
        .iter()
        .enumerate()
        .map(|(i, option)| {
            let style = if i == selected {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(Span::styled(format!(" {option} "), style)))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Ctrl+P — New "))
        .highlight_style(Style::default().add_modifier(Modifier::BOLD));
    frame.render_widget(Clear, rect);
    frame.render_widget(list, rect);
}

fn draw_form(frame: &mut Frame, form: &Form) {
    let area = frame.area();
    let rect = centered_rect(area, 70, form.fields.len() as u16 + 4);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        form.title,
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));
    for (i, field) in form.fields.iter().enumerate() {
        if i < form.current {
            lines.push(Line::from(Span::styled(
                format!("  ✓ {field}: {}", form.values[i]),
                Style::default().fg(Color::Green),
            )));
        } else if i == form.current {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("> {field}: "),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::raw(form.input.as_str()),
                Span::styled("█", Style::default().fg(Color::Cyan)),
            ]));
        } else {
            lines.push(Line::from(format!("  {field}")));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Enter: next · Esc: cancel ",
        Style::default().fg(Color::DarkGray),
    )));

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Step-by-step form "),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(Clear, rect);
    frame.render_widget(paragraph, rect);
}

fn draw_catalog(frame: &mut Frame, app: &App, area: Rect) {
    let widths = [
        Constraint::Length(26),
        Constraint::Length(26),
        Constraint::Length(20),
        Constraint::Min(0),
    ];
    let header = Row::new(vec!["NAME", "MODEL", "SCHEDULE", "NEEDS"])
        .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = app
        .catalog
        .iter()
        .map(|e| {
            Row::new(vec![
                Cell::from(e.name.clone()),
                Cell::from(e.model.clone()),
                Cell::from(e.schedule.clone().unwrap_or_default()),
                Cell::from(e.needs.clone().unwrap_or_default()),
            ])
        })
        .collect();

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(
                    " Catalog ({}) — ↑/↓ select, Enter to start ",
                    app.catalog.len()
                )),
        )
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .column_spacing(2);

    let mut state = TableState::default();
    state.select(Some(app.catalog_selected));
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_schedules(frame: &mut Frame, app: &App, area: Rect) {
    let widths = [
        Constraint::Length(10),
        Constraint::Length(20),
        Constraint::Length(26),
        Constraint::Length(12),
        Constraint::Min(0),
    ];
    let header = Row::new(vec!["ID", "CRON", "TASK", "ENABLED", "INPUT"])
        .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = app
        .schedules
        .iter()
        .map(|s| {
            let input = s.input.to_string();
            let input = if input.chars().count() > 40 {
                format!("{}…", input.chars().take(40).collect::<String>())
            } else {
                input
            };
            Row::new(vec![
                Cell::from(short_str(&s.id)),
                Cell::from(s.cron.clone()),
                Cell::from(s.task.clone()),
                Cell::from(if s.enabled { "yes" } else { "no" }),
                Cell::from(input),
            ])
        })
        .collect();

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Schedules ({}) ", app.schedules.len())),
        )
        .column_spacing(2);
    frame.render_widget(table, area);
}

fn draw_notifications(frame: &mut Frame, app: &App, area: Rect) {
    let widths = [
        Constraint::Length(8),
        Constraint::Length(12),
        Constraint::Length(10),
        Constraint::Length(28),
        Constraint::Min(0),
    ];
    let header = Row::new(vec!["ID", "CHANNEL", "STATUS", "SUBJECT", "BODY"])
        .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = app
        .notifications
        .iter()
        .map(|n| {
            let body = n.body.chars().take(40).collect::<String>();
            Row::new(vec![
                Cell::from(n.id.to_string()),
                Cell::from(n.channel.clone()),
                Cell::from(status_ok(&n.status)),
                Cell::from(n.subject.clone()),
                Cell::from(body),
            ])
        })
        .collect();

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Notifications ({}) ", app.notifications.len())),
        )
        .column_spacing(2);
    frame.render_widget(table, area);
}

fn status_ok(s: &str) -> Span<'static> {
    let (txt, color) = if s == "ok" {
        ("ok", Color::Green)
    } else {
        ("error", Color::Red)
    };
    Span::styled(txt.to_string(), Style::default().fg(color))
}

fn draw_tasks(frame: &mut Frame, app: &App, area: Rect) {
    let widths = [
        Constraint::Length(10),
        Constraint::Length(22),
        Constraint::Length(12),
        Constraint::Length(8),
        Constraint::Min(0),
    ];
    let header = Row::new(vec!["ID", "TASK", "STATUS", "AGE", "ERROR"])
        .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = app
        .tasks
        .iter()
        .map(|t| {
            Row::new(vec![
                Cell::from(short_id(&t.id)),
                Cell::from(t.name.clone()),
                Cell::from(status_span(t.status)),
                Cell::from(age(t.created_at)),
                Cell::from(t.error.clone().unwrap_or_default()),
            ])
        })
        .collect();

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(
                    " Tasks ({}) — ↑/↓ select, Enter to chat ",
                    app.tasks.len()
                )),
        )
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .column_spacing(2);

    let mut state = TableState::default();
    state.select(Some(app.tasks_selected));
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_chat(frame: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(3)]).split(area);
    let (msg_area, input_area) = (chunks[0], chunks[1]);

    let title = match &app.chat_task_id {
        Some(t) => format!(" Chat · task {t} "),
        None => " Chat ".to_string(),
    };

    let width = msg_area.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();
    let mut thinking_headers: Vec<(usize, usize, usize)> = Vec::new(); // (buffer_line_idx, box_lines, asst_idx)

    let mut asst_idx = 0usize;
    for m in &app.chat_messages {
        if m.role == MessageRole::Assistant {
            if let Some(r) = &m.reasoning {
                let collapsed = app.thinking_collapsed.get(asst_idx).copied().unwrap_or(true);
                let header_line = lines.len();
                let box_lines = push_thinking(&mut lines, r, !collapsed, false, "", width);
                thinking_headers.push((header_line, box_lines, asst_idx));
            }
            asst_idx += 1;
        }
        let (label, color) = role_label_color(m.role);
        lines.push(Line::from(Span::styled(
            format!("[{label}]"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));
        if m.role == MessageRole::Assistant {
            lines.extend(render_markdown(&m.content, width));
        } else {
            for wl in wrap_text(&m.content, width) {
                lines.push(Line::from(Span::styled(wl, Style::default().fg(color))));
            }
        }
        lines.push(Line::from(""));
    }

    // Live turn: streaming reasoning (expanded) and the streamed answer.
    if app.thinking || !app.streaming_reasoning.is_empty() {
        let spinner = if app.thinking { spinner_char(app) } else { "" };
        push_thinking(&mut lines, &app.streaming_reasoning, true, true, spinner, width);
    }
    if let Some(stream) = &app.streaming {
        lines.push(Line::from(Span::styled(
            "[assistant]",
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        )));
        lines.extend(render_markdown(stream, width));
        lines.push(Line::from(""));
    }

    if lines.is_empty() {
        lines.push(Line::from("(no messages — press Enter on a task to open a chat)"));
    }

    // Scroll: `chat_scroll` is the number of lines up from the bottom (0 = pinned).
    let visible = msg_area.height as usize;
    let start = lines.len().saturating_sub(visible + app.chat_scroll);

    // Record click regions for visible thinking headers (and their whole box).
    let col_start = msg_area.x + 1;
    let col_end = col_start + width as u16;
    for (buffer_idx, box_lines, asst_idx) in &thinking_headers {
        if *buffer_idx >= start && *buffer_idx < start + visible {
            let top = msg_area.y + 1 + (*buffer_idx - start) as u16;
            for r in top..top + *box_lines as u16 {
                app.click_regions.push(ClickRegion {
                    row: r,
                    col_start,
                    col_end,
                    action: ClickAction::ToggleThinking(*asst_idx),
                });
            }
        }
    }

    let shown: Vec<Line> = lines.into_iter().skip(start).collect();

    let messages = Paragraph::new(shown)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    frame.render_widget(messages, msg_area);

    let input = format!("> {}", app.input);
    let input_box = Paragraph::new(input).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Message — type, Enter to send, Esc to leave"),
    );
    frame.render_widget(input_box, input_area);
}

/// Append a bordered "Thinking" box (arrow + optional spinner + reasoning) to the
/// chat line buffer. Returns the number of lines added.
fn push_thinking(
    lines: &mut Vec<Line>,
    reasoning: &str,
    expanded: bool,
    active: bool,
    spinner: &str,
    width: usize,
) -> usize {
    let before = lines.len();
    let dim = Style::default().fg(Color::DarkGray);
    let arrow = if expanded { "▾" } else { "▸" };
    let title = if active {
        format!("{spinner} {arrow} Thinking")
    } else {
        format!("{arrow} Thinking")
    };

    // Top border: ┌─ <title> ──…┐
    let inner = width.saturating_sub(2).max(1);
    let mut title_pad = format!(" {title} ");
    if title_pad.chars().count() > inner {
        title_pad = title_pad.chars().take(inner).collect();
    } else {
        title_pad.push_str(&"─".repeat(inner - title_pad.chars().count()));
    }
    lines.push(Line::from(Span::styled(format!("┌{title_pad}┐"), dim)));

    if expanded && !reasoning.is_empty() {
        for wl in wrap_text(reasoning, inner.saturating_sub(2)) {
            let mut padded = wl;
            if padded.chars().count() > inner {
                padded = padded.chars().take(inner).collect();
            } else {
                padded.push_str(&" ".repeat(inner - padded.chars().count()));
            }
            lines.push(Line::from(Span::styled(format!("│{padded}│"), dim)));
        }
    }

    lines.push(Line::from(Span::styled(
        format!("└{}┘", "─".repeat(inner)),
        dim,
    )));

    lines.len() - before
}

fn role_label_color(role: MessageRole) -> (&'static str, Color) {
    match role {
        MessageRole::System => ("system", Color::DarkGray),
        MessageRole::User => ("you", Color::Cyan),
        MessageRole::Assistant => ("assistant", Color::Green),
        MessageRole::Tool => ("tool", Color::Yellow),
    }
}

/// The current frame of the BRAILLE_EIGHT throbber set.
fn spinner_char(app: &App) -> &'static str {
    let set = throbber_widgets_tui::BRAILLE_EIGHT;
    let len = set.symbols.len() as i8;
    let idx = app.throbber_state.index().rem_euclid(len) as usize;
    set.symbols[idx]
}

/// Compute the (tab, start column, end column) triplets for the tab bar, matching
/// how the `Tabs` widget lays titles out.
fn tab_click_regions(x0: u16, _y0: u16) -> Vec<(Tab, u16, u16)> {
    let mut x = x0 + 1;
    let mut out = Vec::new();
    for tab in Tab::ALL {
        let width = tab.label().chars().count() as u16;
        out.push((tab, x, x + width));
        x += width + 3;
    }
    out
}

/// Word-wrap `text` into lines of at most `width` characters.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current = word.to_string();
        } else if current.len() + 1 + word.len() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            current = word.to_string();
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Render Markdown text into styled ratatui lines. Handles headings, lists,
/// blockquotes, fenced code blocks, and inline `**bold**` / `*italic*` / `` `code` ``.
fn render_markdown(text: &str, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut in_code_block = false;

    for raw in text.lines() {
        let line = raw.trim_end();

        if line.trim_start().starts_with("```") {
            in_code_block = !in_code_block;
            continue;
        }
        if in_code_block {
            lines.push(Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(Color::Yellow),
            )));
            continue;
        }

        if let Some(rest) = line.strip_prefix("#### ") {
            push_heading(&mut lines, rest, width, Style::default().add_modifier(Modifier::BOLD));
        } else if let Some(rest) = line.strip_prefix("### ") {
            push_heading(
                &mut lines,
                rest,
                width,
                Style::default().fg(Color::LightCyan).add_modifier(Modifier::BOLD),
            );
        } else if let Some(rest) = line.strip_prefix("## ") {
            push_heading(
                &mut lines,
                rest,
                width,
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            );
        } else if let Some(rest) = line.strip_prefix("# ") {
            push_heading(
                &mut lines,
                rest,
                width,
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            );
        } else if let Some(rest) = line.strip_prefix("> ") {
            push_blockquote(&mut lines, rest, width);
        } else if let Some((marker, rest)) = list_item(line) {
            push_list_item(&mut lines, &marker, rest, width);
        } else if line.trim().is_empty() {
            lines.push(Line::from(""));
        } else {
            for w in wrap_text(line, width) {
                lines.push(Line::from(inline_spans(&w, Style::default())));
            }
        }
    }

    lines
}

fn push_heading(lines: &mut Vec<Line<'static>>, text: &str, width: usize, style: Style) {
    for w in wrap_text(text, width) {
        lines.push(Line::from(Span::styled(w, style)));
    }
}

fn push_blockquote(lines: &mut Vec<Line<'static>>, text: &str, width: usize) {
    let style = Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC);
    let inner = width.saturating_sub(2).max(1);
    for w in wrap_text(text, inner) {
        lines.push(Line::from(inline_spans(&format!("│ {w}"), style)));
    }
}

fn push_list_item(lines: &mut Vec<Line<'static>>, marker: &str, text: &str, width: usize) {
    let prefix = format!("  {marker} ");
    let cont = " ".repeat(prefix.chars().count());
    let inner = width.saturating_sub(prefix.chars().count()).max(1);
    let wrapped = wrap_text(text, inner);
    if wrapped.is_empty() {
        lines.push(Line::from(inline_spans(&prefix, Style::default())));
        return;
    }
    for (i, w) in wrapped.into_iter().enumerate() {
        let p = if i == 0 { prefix.as_str() } else { cont.as_str() };
        lines.push(Line::from(inline_spans(&format!("{p}{w}"), Style::default())));
    }
}

/// Detect a list item prefix: `- `/`* `/`+ ` bullets or `N. ` numbers.
/// Returns the rendered marker and the remaining content.
fn list_item(line: &str) -> Option<(String, &str)> {
    for b in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(b) {
            return Some(("•".to_string(), rest));
        }
    }
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i > 0 && i < bytes.len() && bytes[i] == b'.' && i + 1 < bytes.len() && bytes[i + 1] == b' ' {
        return Some((line[..i + 1].to_string(), &line[i + 2..]));
    }
    None
}

/// Parse inline `**bold**`, `*italic*`, and `` `code` `` into styled spans.
fn inline_spans(text: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                flush_inline(&mut buf, base, &mut spans);
                let (inner, closed) = take_until_double(&mut chars, '*');
                if closed {
                    spans.push(Span::styled(inner, base.add_modifier(Modifier::BOLD)));
                } else {
                    spans.push(Span::styled(format!("**{inner}"), base));
                }
            }
            '*' => {
                flush_inline(&mut buf, base, &mut spans);
                let (inner, closed) = take_until_char(&mut chars, '*');
                if closed {
                    spans.push(Span::styled(inner, base.add_modifier(Modifier::ITALIC)));
                } else {
                    spans.push(Span::styled(format!("*{inner}"), base));
                }
            }
            '`' => {
                flush_inline(&mut buf, base, &mut spans);
                let (inner, closed) = take_until_char(&mut chars, '`');
                if closed {
                    spans.push(Span::styled(inner, base.fg(Color::Yellow)));
                } else {
                    spans.push(Span::styled(format!("`{inner}"), base));
                }
            }
            _ => buf.push(c),
        }
    }
    flush_inline(&mut buf, base, &mut spans);
    spans
}

fn flush_inline(buf: &mut String, base: Style, spans: &mut Vec<Span<'static>>) {
    if !buf.is_empty() {
        spans.push(Span::styled(std::mem::take(buf), base));
    }
}

fn take_until_char(chars: &mut std::iter::Peekable<std::str::Chars>, ch: char) -> (String, bool) {
    let mut s = String::new();
    for c in chars.by_ref() {
        if c == ch {
            return (s, true);
        }
        s.push(c);
    }
    (s, false)
}

fn take_until_double(chars: &mut std::iter::Peekable<std::str::Chars>, ch: char) -> (String, bool) {
    let mut s = String::new();
    while let Some(c) = chars.next() {
        if c == ch && chars.peek() == Some(&ch) {
            chars.next();
            return (s, true);
        }
        s.push(c);
    }
    (s, false)
}

fn draw_events(frame: &mut Frame, app: &App, area: Rect) {
    let widths = [
        Constraint::Length(8),
        Constraint::Length(18),
        Constraint::Length(10),
        Constraint::Min(0),
    ];
    let header = Row::new(vec!["ID", "KIND", "AGE", "PAYLOAD"])
        .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));

    let max_rows = area.height.saturating_sub(3) as usize;
    let mut rows: Vec<Row> = app
        .events
        .iter()
        .rev()
        .take(max_rows)
        .map(|e| {
            let payload = e.payload.to_string();
            let payload = if payload.chars().count() > 60 {
                format!("{}…", payload.chars().take(60).collect::<String>())
            } else {
                payload
            };
            Row::new(vec![
                Cell::from(e.id.to_string()),
                Cell::from(e.kind.as_str()),
                Cell::from(age(e.created_at)),
                Cell::from(payload),
            ])
        })
        .collect();
    if rows.is_empty() {
        rows.push(Row::new(vec![
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
            Cell::from("(no events yet)"),
        ]));
    }

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Events ({}) ", app.events.len())),
        )
        .column_spacing(2);
    frame.render_widget(table, area);
}

fn draw_placeholder(frame: &mut Frame, app: &App, area: Rect) {
    let msg = format!("The {} tab arrives in a later milestone.", app.tab.label());
    let p = Paragraph::new(msg).block(
        Block::default()
            .borders(Borders::ALL)
            .title(app.tab.label()),
    );
    frame.render_widget(p, area);
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let (state_txt, color) = match app.conn {
        ConnState::Connected => ("connected", Color::Green),
        ConnState::Connecting => ("connecting", Color::Yellow),
        ConnState::Disconnected => ("disconnected", Color::Red),
    };

    let left = format!(" {state_txt} · {}", app.conn_detail);
    let right = format!(
        "tasks: {}  events: {}  [↑↓] select  [Enter] chat  [Tab] switch  [q] quit",
        app.tasks.len(),
        app.events.len()
    );

    let line = Line::from(vec![
        Span::styled(left, Style::default().fg(color)),
        Span::raw(" "),
        Span::raw(right),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn status_span(s: TaskStatus) -> Span<'static> {
    let (txt, color) = match s {
        TaskStatus::Pending => ("pending", Color::Yellow),
        TaskStatus::Running => ("running", Color::Cyan),
        TaskStatus::Succeeded => ("succeeded", Color::Green),
        TaskStatus::Failed => ("failed", Color::Red),
        TaskStatus::Cancelled => ("cancelled", Color::DarkGray),
    };
    Span::styled(txt.to_string(), Style::default().fg(color))
}

fn short_id(id: &uuid::Uuid) -> String {
    id.to_string().split('-').next().unwrap_or("?").to_string()
}

fn short_str(id: &str) -> String {
    id.chars().take(8).collect()
}

fn age(ts: DateTime<Utc>) -> String {
    let secs = (Utc::now() - ts).num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect::<String>()
    }

    #[test]
    fn thinking_box_has_border_and_uniform_width() {
        let mut lines = Vec::new();
        let width = 40usize;
        push_thinking(&mut lines, "hello world this is reasoning", true, false, "", width);
        let texts: Vec<String> = lines.iter().map(line_text).collect();

        assert!(texts.len() >= 3, "expected top + body + bottom, got {texts:?}");
        assert!(texts[0].starts_with('┌') && texts[0].ends_with('┐'));
        assert!(texts[0].contains("▾ Thinking"));
        assert!(texts.last().unwrap().starts_with('└'));
        for t in &texts {
            assert_eq!(t.chars().count(), width, "box line width mismatch: {t:?}");
        }
        for t in &texts[1..texts.len() - 1] {
            assert!(t.starts_with('│') && t.ends_with('│'), "body line: {t:?}");
        }
    }

    #[test]
    fn collapsed_thinking_box_is_compact() {
        let mut lines = Vec::new();
        push_thinking(&mut lines, "", false, false, "", 40);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(texts.len(), 2);
        assert!(texts[0].contains("▸ Thinking"));
    }

    #[test]
    fn tab_regions_map_columns_correctly() {
        let regions = tab_click_regions(0, 0);
        let find = |col: u16| {
            regions
                .iter()
                .find(|(_, s, e)| col >= *s && col < *e)
                .map(|(t, _, _)| *t)
        };
        assert_eq!(find(2), Some(Tab::Tasks));
        assert_eq!(find(10), Some(Tab::Catalog));
        assert_eq!(find(20), Some(Tab::Chat));
        assert_eq!(find(27), Some(Tab::Events));
        assert_eq!(find(81), Some(Tab::Mcp));
    }

    #[test]
    fn thinking_click_region_matches_render() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = App::new();
        app.tab = Tab::Chat;
        app.chat_messages = vec![favetto_core::model::ChatMessage {
            role: MessageRole::Assistant,
            content: "answer".to_string(),
            reasoning: Some("reasoning text".to_string()),
        }];
        app.sync_collapse();

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();

        let region = app
            .click_regions
            .iter()
            .find(|r| matches!(r.action, ClickAction::ToggleThinking(_)))
            .expect("no thinking click region recorded");

        let buffer = terminal.backend().buffer();
        let cell = buffer
            .cell((region.col_start, region.row))
            .expect("region out of bounds");
        assert_eq!(cell.symbol(), "┌", "thinking header should start with a box corner");
    }

    #[test]
    fn clicking_thinking_region_toggles_collapse() {
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

        let mut app = App::new();
        app.chat_messages = vec![favetto_core::model::ChatMessage {
            role: MessageRole::Assistant,
            content: "answer".to_string(),
            reasoning: Some("reasoning".to_string()),
        }];
        app.sync_collapse();
        app.click_regions.push(ClickRegion {
            row: 3,
            col_start: 1,
            col_end: 79,
            action: ClickAction::ToggleThinking(0),
        });

        assert!(app.thinking_collapsed[0]);
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 3,
            modifiers: KeyModifiers::empty(),
        });
        assert!(!app.thinking_collapsed[0], "click should expand the thinking box");
    }

    #[test]
    fn markdown_renders_headings_lists_and_inline() {
        let out = render_markdown("# Title\n\n- one\n- **two**\n\n1. first\n2. `code`\n", 40);
        let texts: Vec<String> = out.iter().map(line_text).collect();

        assert!(texts.iter().any(|t| t == "Title"), "heading: {texts:?}");
        assert!(texts.iter().any(|t| t == "  • one"), "bullet: {texts:?}");
        assert!(texts.iter().any(|t| t == "  • two"), "bold bullet: {texts:?}");
        assert!(texts.iter().any(|t| t == "  1. first"), "numbered: {texts:?}");
        assert!(texts.iter().any(|t| t == "  2. code"), "numbered code: {texts:?}");

        let bold_span = out
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.as_ref() == "two")
            .expect("bold span missing");
        assert!(bold_span.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn markdown_renders_code_blocks_and_quotes() {
        let out = render_markdown("```\nlet x = 1;\n```\n\n> quoted\n", 40);
        let texts: Vec<String> = out.iter().map(line_text).collect();
        assert!(texts.iter().any(|t| t == "let x = 1;"), "code body: {texts:?}");
        assert!(texts.iter().any(|t| t == "│ quoted"), "quote: {texts:?}");
        assert!(!texts.iter().any(|t| t.contains("```")), "fences stripped: {texts:?}");
    }
}
