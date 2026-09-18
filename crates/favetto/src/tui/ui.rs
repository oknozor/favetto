//! ratatui rendering: header tabs, per-tab content, and a connection status bar.

use chrono::{DateTime, Utc};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Tabs, Wrap};
use ratatui::Frame;

use favetto_core::model::{MessageRole, TaskStatus};

use super::app::{App, ConnState, Tab};

pub fn draw(frame: &mut Frame, app: &App) {
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(frame.area());

    let titles: Vec<Line> = Tab::ALL
        .iter()
        .map(|t| Line::from(Span::raw(t.label())))
        .collect();
    let tabs = Tabs::new(titles)
        .select(Tab::ALL.iter().position(|t| *t == app.tab).unwrap_or(0))
        .block(Block::default().borders(Borders::BOTTOM))
        .highlight_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
    frame.render_widget(tabs, chunks[0]);

    match app.tab {
        Tab::Tasks => draw_tasks(frame, app, chunks[1]),
        Tab::Chat => draw_chat(frame, app, chunks[1]),
        Tab::Events => draw_events(frame, app, chunks[1]),
        Tab::Scheduler => draw_schedules(frame, app, chunks[1]),
        Tab::Notifications => draw_notifications(frame, app, chunks[1]),
        _ => draw_placeholder(frame, app, chunks[1]),
    }

    draw_status(frame, app, chunks[2]);
}

fn draw_schedules(frame: &mut Frame, app: &App, area: Rect) {
    let widths = [
        Constraint::Length(10),
        Constraint::Length(20),
        Constraint::Length(26),
        Constraint::Length(12),
        Constraint::Min(0),
    ];
    let header = Row::new(vec!["ID", "CRON", "SKILL", "ENABLED", "INPUT"])
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
                Cell::from(s.skill.clone()),
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
        Constraint::Length(26),
        Constraint::Length(12),
        Constraint::Length(10),
        Constraint::Min(0),
    ];
    let header = Row::new(vec!["ID", "SKILL", "STATUS", "AGE", "ERROR"])
        .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = app
        .tasks
        .iter()
        .map(|t| {
            Row::new(vec![
                Cell::from(short_id(&t.id)),
                Cell::from(t.skill.clone()),
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

fn draw_chat(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(3)]).split(area);

    let title = match &app.chat_task_id {
        Some(t) => format!(" Chat · task {t} "),
        None => " Chat ".to_string(),
    };

    let mut lines: Vec<Line> = Vec::new();
    for m in app.chat_messages.iter().rev().take(200) {
        let (label, color) = match m.role {
            MessageRole::System => ("system", Color::DarkGray),
            MessageRole::User => ("you", Color::Cyan),
            MessageRole::Assistant => ("assistant", Color::Green),
            MessageRole::Tool => ("tool", Color::Yellow),
        };
        lines.push(Line::from(Span::styled(
            format!("[{label}] {}", m.content),
            Style::default().fg(color),
        )));
        lines.push(Line::from(""));
    }
    lines.reverse();

    if lines.is_empty() {
        lines.push(Line::from("(no messages — press Enter on a task to open a chat)"));
    }

    let messages = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    frame.render_widget(messages, chunks[0]);

    let input = format!("> {}", app.input);
    let input_box = Paragraph::new(input).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Message — type, Enter to send, Esc to leave"),
    );
    frame.render_widget(input_box, chunks[1]);
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
            Cell::from("(no events yet — start the daemon to see synthetic events)"),
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
