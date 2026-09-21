//! ratatui rendering: header tabs, per-tab content, and a connection status bar.

use chrono::{DateTime, Utc};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table, TableState, Tabs, Wrap,
};
use ratatui::Frame;

use favetto_core::model::TaskStatus;

use super::app::{
    table_rows_area, App, ClickAction, ClickRegion, ConnState, Form, ListGeometry, Popup, Tab,
    TaskVarsForm, Wizard, WizardStep, MENU_OPTIONS,
};
use super::{json, markdown};

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
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_widget(tabs, chunks[0]);

    let tab = app.tab;
    match tab {
        Tab::Tasks => draw_tasks(frame, app, chunks[1]),
        Tab::Catalog => draw_catalog(frame, app, chunks[1]),
        Tab::Agent => draw_agent(frame, app, chunks[1]),
        Tab::Events => draw_events(frame, app, chunks[1]),
        Tab::Scheduler => draw_schedules(frame, app, chunks[1]),
        Tab::Notifications => draw_notifications(frame, app, chunks[1]),
    }

    draw_status(frame, app, chunks[2]);

    if !matches!(app.popup, Popup::None) {
        draw_popup(frame, app);
    }
}

fn draw_popup(frame: &mut Frame, app: &mut App) {
    match &mut app.popup {
        Popup::None => {}
        Popup::Menu { selected } => draw_menu(frame, *selected),
        Popup::Form(form) => draw_form(frame, form),
        Popup::Wizard(wizard) => draw_wizard(frame, wizard),
        Popup::TaskVars(form) => draw_task_vars(frame, form),
        Popup::Help { scroll } => draw_help(frame, scroll),
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
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Ctrl+P — New "),
        )
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
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
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

/// Number of list rows the wizard shows at once.
const WIZARD_VISIBLE: usize = 12;

fn draw_wizard(frame: &mut Frame, wizard: &Wizard) {
    let area = frame.area();
    let rows = wizard.choices.len().min(WIZARD_VISIBLE) as u16;
    let rect = centered_rect(area, 70, rows + 6);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        wizard.step.title(),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    if wizard.step == WizardStep::Dir {
        lines.push(Line::from(vec![
            Span::styled("> Directory: ", Style::default().fg(Color::Cyan)),
            Span::raw(wizard.dir.as_str()),
            Span::styled("█", Style::default().fg(Color::Cyan)),
        ]));
    } else if wizard.loading {
        lines.push(Line::from(Span::styled(
            "loading…",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        let start = wizard.selected.saturating_sub(WIZARD_VISIBLE - 1);
        for (idx, (label, _)) in wizard
            .choices
            .iter()
            .enumerate()
            .skip(start)
            .take(WIZARD_VISIBLE)
        {
            let style = if idx == wizard.selected {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default()
            };
            lines.push(Line::from(Span::styled(format!(" {label} "), style)));
        }
        if wizard.choices.is_empty() {
            lines.push(Line::from(Span::styled(
                "(nothing available)",
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    if let Some(err) = &wizard.error {
        lines.push(Line::from(Span::styled(
            err.as_str(),
            Style::default().fg(Color::Red),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " ↑/↓ select · Enter: next · Esc: cancel ",
        Style::default().fg(Color::DarkGray),
    )));

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" New one-shot task "),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(Clear, rect);
    frame.render_widget(paragraph, rect);
}

fn draw_task_vars(frame: &mut Frame, form: &TaskVarsForm) {
    let area = frame.area();
    let rect = centered_rect(area, 70, form.vars.len() as u16 + 6);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!(" Task input · {} ", form.task),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    for (i, var) in form.vars.iter().enumerate() {
        let current = i == form.current;
        let label = format!("{}{}: ", if current { "> " } else { "  " }, var.prompt);
        let label_style = if current {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        if let Some(choices) = &var.choices {
            lines.push(Line::from(Span::styled(label, label_style)));
            let selected = form.choice_selected.get(i).copied().unwrap_or(0);
            for (j, choice) in choices.iter().enumerate() {
                let style = if j == selected {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                } else {
                    Style::default()
                };
                lines.push(Line::from(Span::styled(format!("    {choice}"), style)));
            }
            continue;
        }

        let value = form.values.get(i).cloned().unwrap_or_default();
        let segments: Vec<&str> = value.split('\n').collect();
        let last_segment = segments.len() - 1;
        for (k, segment) in segments.iter().enumerate() {
            let mut spans: Vec<Span> = Vec::new();
            if k == 0 {
                spans.push(Span::styled(label.clone(), label_style));
            } else {
                spans.push(Span::raw("    "));
            }
            if segment.is_empty() && !current && segments.len() == 1 {
                spans.push(Span::styled(
                    "(empty)",
                    Style::default().fg(Color::DarkGray),
                ));
            } else {
                spans.push(Span::raw((*segment).to_string()));
            }
            if current && k == last_segment {
                spans.push(Span::styled("█", Style::default().fg(Color::Cyan)));
            }
            lines.push(Line::from(spans));
        }
    }

    if let Some(err) = &form.error {
        lines.push(Line::from(Span::styled(
            err.as_str(),
            Style::default().fg(Color::Red),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Tab/↑↓ move · Enter next · Ctrl+Enter submit · Esc cancel ",
        Style::default().fg(Color::DarkGray),
    )));

    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" Task input "))
        .wrap(Wrap { trim: false });
    frame.render_widget(Clear, rect);
    frame.render_widget(paragraph, rect);
}

/// Grouped keybinding reference shown by `?`. Each row is `(key, description)`.
const HELP_SECTIONS: &[(&str, &[(&str, &str)])] = &[
    (
        "Global",
        &[
            ("Tab / →", "next tab"),
            ("Shift+Tab / ←", "previous tab"),
            ("Ctrl+P", "open the action menu"),
            ("?", "open/close this help"),
            ("M", "mute/unmute sound"),
            ("q / Esc", "quit"),
        ],
    ),
    (
        "Lists (Tasks, Catalog, Events, Scheduler, Notifications)",
        &[
            ("↑ / ↓", "move selection"),
            ("PageUp / PageDown", "move/scroll by a page"),
        ],
    ),
    (
        "Tasks tab",
        &[("Enter", "open the selected task's agent session")],
    ),
    (
        "Catalog tab",
        &[
            ("Enter", "start the selected catalog task"),
            ("PageUp / PageDown / wheel", "scroll the preview pane"),
        ],
    ),
    (
        "Agent tab",
        &[
            ("Ctrl+Y", "toggle focus: agent ↔ favetto"),
            ("(agent focus)", "every key is forwarded to the agent"),
            ("(favetto focus) Ctrl+Q", "leave the panel"),
            ("(favetto focus) Ctrl+N", "start a new session"),
            ("(favetto focus) Esc", "back to Tasks"),
        ],
    ),
    (
        "Popups (menu / form / wizard)",
        &[
            ("↑ / ↓", "select"),
            ("Enter", "confirm / next field"),
            ("Esc", "cancel / close"),
        ],
    ),
    (
        "Mouse",
        &[
            ("click the tab bar", "switch tabs"),
            ("(Agent) wheel/click", "forwarded when the agent enables it"),
        ],
    ),
];

/// Flatten [`HELP_SECTIONS`] into the styled lines rendered by [`draw_help`].
fn help_lines() -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    for (section, rows) in HELP_SECTIONS {
        lines.push(Line::from(Span::styled(
            (*section).to_string(),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )));
        for (k, v) in *rows {
            lines.push(Line::from(vec![
                Span::styled(format!("  {k} — "), Style::default().fg(Color::Yellow)),
                Span::raw((*v).to_string()),
            ]));
        }
        lines.push(Line::from(""));
    }
    lines
}

/// Draw the scrollable `?` keybinding overlay, clamping `scroll` to the content.
fn draw_help(frame: &mut Frame, scroll: &mut u16) {
    let area = frame.area();
    let rect = centered_rect(area, 80, area.height.saturating_sub(2));

    let paragraph = Paragraph::new(help_lines())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Help (?) — ↑/↓ scroll, Esc/? close "),
        )
        .wrap(Wrap { trim: false });
    let inner_width = rect.width.saturating_sub(2);
    let inner_height = rect.height.saturating_sub(2);
    // `line_count` returns `usize`; the preview pane uses the same conversion.
    let max_scroll = (paragraph.line_count(inner_width) as u16).saturating_sub(inner_height);
    *scroll = (*scroll).min(max_scroll);

    frame.render_widget(Clear, rect);
    frame.render_widget(paragraph.scroll((*scroll, 0)), rect);
}

fn draw_catalog(frame: &mut Frame, app: &mut App, area: Rect) {
    let chunks =
        Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).split(area);
    draw_catalog_table(frame, app, chunks[0]);
    draw_catalog_preview(frame, app, chunks[1]);
}

fn draw_catalog_table(frame: &mut Frame, app: &mut App, area: Rect) {
    let widths = [
        Constraint::Min(14),
        Constraint::Length(10),
        Constraint::Length(18),
        Constraint::Min(0),
    ];
    let header = Row::new(vec!["NAME", "AGENT", "MODEL", "NEEDS"]).style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );

    let rows: Vec<Row> = app
        .catalog
        .iter()
        .map(|e| {
            Row::new(vec![
                Cell::from(e.name.clone()),
                Cell::from(e.agent.clone().unwrap_or_else(|| "—".to_string())),
                Cell::from(e.model_display()),
                Cell::from(e.needs.clone().unwrap_or_default()),
            ])
        })
        .collect();

    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(format!(
            " Catalog ({}) — Enter/click to start ",
            app.catalog.len()
        )))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .column_spacing(1);

    let mut state = TableState::default();
    state.select(Some(app.catalog_selected));
    frame.render_stateful_widget(table, area, &mut state);
    let len = app.catalog.len();
    app.catalog_geom = ListGeometry {
        inner: table_rows_area(area),
        offset: state.offset(),
        len,
    };
}

fn draw_catalog_preview(frame: &mut Frame, app: &mut App, area: Rect) {
    // Remember the pane's rectangle so the mouse wheel can be targeted at it.
    app.catalog_preview_area = Some(area);

    let (name, lines) = match app.catalog_preview.as_ref() {
        Some((name, markdown)) if !markdown.trim().is_empty() => {
            (Some(name.as_str()), markdown::render_task(markdown))
        }
        Some((name, _)) => (
            Some(name.as_str()),
            vec![Line::from(Span::styled(
                "(no content)",
                Style::default().fg(Color::DarkGray),
            ))],
        ),
        None => (
            None,
            vec![Line::from(Span::styled(
                "Select a task to preview its definition.",
                Style::default().fg(Color::DarkGray),
            ))],
        ),
    };

    // Measure the wrapped content so the scroll offset can be clamped to it.
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let inner_width = area.width.saturating_sub(2);
    let inner_height = area.height.saturating_sub(2);
    let line_count = paragraph.line_count(inner_width) as u16;
    app.catalog_preview_max_scroll = line_count.saturating_sub(inner_height);
    app.catalog_preview_scroll = app
        .catalog_preview_scroll
        .min(app.catalog_preview_max_scroll);

    let hint = if app.catalog_preview_max_scroll > 0 {
        " · PgUp/PgDn or wheel to scroll"
    } else {
        ""
    };
    let title = match name {
        Some(name) => format!(" Preview · {name}{hint} "),
        None => format!(" Preview{hint} "),
    };

    let paragraph = paragraph
        .block(Block::default().borders(Borders::ALL).title(title))
        .scroll((app.catalog_preview_scroll, 0));
    frame.render_widget(paragraph, area);
}

fn draw_schedules(frame: &mut Frame, app: &mut App, area: Rect) {
    let widths = [
        Constraint::Length(10),
        Constraint::Length(20),
        Constraint::Length(26),
        Constraint::Length(12),
        Constraint::Min(0),
    ];
    let header = Row::new(vec!["ID", "CRON", "TASK", "ENABLED", "INPUT"]).style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );

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
        .block(Block::default().borders(Borders::ALL).title(format!(
            " Schedules ({}) — click or ↑/↓ select ",
            app.schedules.len()
        )))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .column_spacing(2);

    let mut state = TableState::default();
    if !app.schedules.is_empty() {
        state.select(Some(app.schedules_selected.min(app.schedules.len() - 1)));
    }
    frame.render_stateful_widget(table, area, &mut state);
    let len = app.schedules.len();
    app.schedules_geom = ListGeometry {
        inner: table_rows_area(area),
        offset: state.offset(),
        len,
    };
}

fn draw_notifications(frame: &mut Frame, app: &mut App, area: Rect) {
    let widths = [
        Constraint::Length(8),
        Constraint::Length(12),
        Constraint::Length(10),
        Constraint::Length(28),
        Constraint::Min(0),
    ];
    let header = Row::new(vec!["ID", "CHANNEL", "STATUS", "SUBJECT", "BODY"]).style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );

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
        .block(Block::default().borders(Borders::ALL).title(format!(
            " Notifications ({}) — click or ↑/↓ select ",
            app.notifications.len()
        )))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .column_spacing(2);

    let mut state = TableState::default();
    if !app.notifications.is_empty() {
        state.select(Some(
            app.notifications_selected.min(app.notifications.len() - 1),
        ));
    }
    frame.render_stateful_widget(table, area, &mut state);
    let len = app.notifications.len();
    app.notifications_geom = ListGeometry {
        inner: table_rows_area(area),
        offset: state.offset(),
        len,
    };
}

fn status_ok(s: &str) -> Span<'static> {
    let (txt, color) = if s == "ok" {
        ("ok", Color::Green)
    } else {
        ("error", Color::Red)
    };
    Span::styled(txt.to_string(), Style::default().fg(color))
}

fn draw_tasks(frame: &mut Frame, app: &mut App, area: Rect) {
    let widths = [
        Constraint::Length(10),
        Constraint::Length(22),
        Constraint::Length(12),
        Constraint::Length(8),
        Constraint::Min(0),
    ];
    let header = Row::new(vec!["ID", "TASK", "STATUS", "AGE", "ERROR"]).style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );

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
        .block(Block::default().borders(Borders::ALL).title(format!(
            " Tasks ({}) — click or ↑/↓ select, Enter/click to run agent ",
            app.tasks.len()
        )))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .column_spacing(2);

    let mut state = TableState::default();
    state.select(Some(app.tasks_selected));
    frame.render_stateful_widget(table, area, &mut state);
    let len = app.tasks.len();
    app.tasks_geom = ListGeometry {
        inner: table_rows_area(area),
        offset: state.offset(),
        len,
    };
}

fn draw_agent(frame: &mut Frame, app: &mut App, area: Rect) {
    let inner = Block::default().borders(Borders::ALL);
    let inner_area = inner.inner(area);
    // Remember the terminal's screen rectangle so mouse events can be translated
    // into the agent's coordinate space.
    app.agent_area = Some(inner_area);

    // Keep the emulator (and via `agent_resize`, the daemon-side PTY) in sync with
    // the panel size.
    let (rows, cols) = (inner_area.height, inner_area.width);
    if rows > 0 && cols > 0 && app.term.size() != (rows, cols) {
        app.term.resize(rows, cols);
        app.agent_resize = Some((rows, cols));
    }

    let title = if app.agent_session_id.is_none() {
        " Agent — no session (Enter on a task) ".to_string()
    } else {
        let name = app.agent_name.as_deref().unwrap_or("agent");
        let state = if app.agent_running {
            "running"
        } else {
            "stopped"
        };
        let extra = if app.agent_status.is_empty() {
            String::new()
        } else {
            format!(" · {}", app.agent_status)
        };
        format!(" Agent · {name} · {state}{extra} · Ctrl+N new · Ctrl+Q leave ")
    };

    let block = Block::default().borders(Borders::ALL).title(title);
    frame.render_widget(block, area);

    if app.agent_session_id.is_none() || inner_area.width == 0 || inner_area.height == 0 {
        let hint = Paragraph::new(Line::from(Span::styled(
            "The embedded agent terminal appears here. Select a task and press Enter to launch the configured agent.",
            Style::default().fg(Color::DarkGray),
        )))
        .wrap(Wrap { trim: true });
        frame.render_widget(hint, inner_area);
        return;
    }

    let screen = app.term.screen();
    let (screen_rows, screen_cols) = screen.size();
    let hide_cursor = screen.hide_cursor();
    let (cursor_row, cursor_col) = screen.cursor_position();

    let mut lines: Vec<Line> = Vec::with_capacity(screen_rows as usize);
    for row in 0..screen_rows.min(inner_area.height) {
        let mut spans: Vec<Span> = Vec::new();
        let mut run = String::new();
        let mut run_style: Option<Style> = None;
        for col in 0..screen_cols.min(inner_area.width) {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let style = cell_style(cell);
            let contents = cell.contents();
            let text = if contents.is_empty() { " " } else { contents };
            if run_style == Some(style) {
                run.push_str(text);
            } else {
                if let Some(prev) = run_style.take() {
                    spans.push(Span::styled(std::mem::take(&mut run), prev));
                }
                run.push_str(text);
                run_style = Some(style);
            }
        }
        if let Some(prev) = run_style {
            spans.push(Span::styled(run, prev));
        }
        lines.push(Line::from(spans));
    }

    frame.render_widget(Paragraph::new(lines), inner_area);

    // Draw the terminal cursor when the application shows one and it is visible.
    if !hide_cursor && cursor_row < inner_area.height && cursor_col < inner_area.width {
        frame.set_cursor_position((inner_area.x + cursor_col, inner_area.y + cursor_row));
    }
}

/// Map a vt100 cell's attributes to a ratatui style.
fn cell_style(cell: &vt100::Cell) -> Style {
    let mut style = Style::default();
    let (mut fg, mut bg) = (vt_color(cell.fgcolor()), vt_color(cell.bgcolor()));

    let mut modifier = Modifier::empty();
    if cell.bold() {
        modifier |= Modifier::BOLD;
    }
    if cell.italic() {
        modifier |= Modifier::ITALIC;
    }
    if cell.underline() {
        modifier |= Modifier::UNDERLINED;
    }
    if cell.dim() {
        modifier |= Modifier::DIM;
    }
    if cell.inverse() {
        std::mem::swap(&mut fg, &mut bg);
    }

    style = style.fg(fg).bg(bg).add_modifier(modifier);
    style
}

/// Convert a vt100 colour to a ratatui colour.
fn vt_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(idx) => Color::Indexed(idx),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
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

fn draw_events(frame: &mut Frame, app: &mut App, area: Rect) {
    let chunks =
        Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).split(area);
    draw_event_list(frame, app, chunks[0]);
    draw_event_payload(frame, app, chunks[1]);
}

fn draw_event_list(frame: &mut Frame, app: &mut App, area: Rect) {
    let widths = [
        Constraint::Length(8),
        Constraint::Min(18),
        Constraint::Length(8),
    ];
    let header = Row::new(vec!["ID", "KIND", "AGE"]).style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );

    let rows: Vec<Row> = app
        .events
        .iter()
        .rev()
        .map(|e| {
            Row::new(vec![
                Cell::from(e.id.to_string()),
                Cell::from(e.kind.as_str()),
                Cell::from(age(e.created_at)),
            ])
        })
        .collect();

    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(format!(
            " Events ({}) — click or ↑/↓ select ",
            app.events.len()
        )))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .column_spacing(1);

    let mut state = TableState::default();
    if !app.events.is_empty() {
        state.select(Some(app.events_selected.min(app.events.len() - 1)));
    }
    frame.render_stateful_widget(table, area, &mut state);
    let len = app.events.len();
    app.events_geom = ListGeometry {
        inner: table_rows_area(area),
        offset: state.offset(),
        len,
    };
}

fn draw_event_payload(frame: &mut Frame, app: &App, area: Rect) {
    let (title, lines) = match app.selected_event() {
        Some(ev) => (
            format!(" Payload · {} #{} ", ev.kind.as_str(), ev.id),
            json::render(&ev.payload),
        ),
        None => (
            " Payload ".to_string(),
            vec![Line::from(Span::styled(
                "(no events yet)",
                Style::default().fg(Color::DarkGray),
            ))],
        ),
    };

    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let (state_txt, color) = match app.conn {
        ConnState::Connected => ("connected", Color::Green),
        ConnState::Connecting => ("connecting", Color::Yellow),
        ConnState::Disconnected => ("disconnected", Color::Red),
    };

    // Keyboard focus: the embedded agent only owns the keyboard while its tab is
    // open and capture is on; otherwise favetto does.
    let agent_focused = app.tab == Tab::Agent && app.agent_capture;
    let (focus_txt, focus_color) = if agent_focused {
        ("agent", Color::Magenta)
    } else {
        ("favetto", Color::Cyan)
    };

    // Sound badge: muted wins visually; a disabled engine shows `sound off`.
    let (sound_txt, sound_color) = if !app.sound_enabled {
        ("sound off", Color::DarkGray)
    } else if app.sound_muted {
        ("muted", Color::Yellow)
    } else {
        ("sound", Color::Green)
    };

    let left = format!(" {state_txt} · {}", app.conn_detail);
    let hint = if app.tab == Tab::Agent {
        if agent_focused {
            "[Ctrl+Y] favetto keys"
        } else {
            "[Ctrl+Y] agent keys  [Ctrl+Q] leave  [Ctrl+N] new  [?] help"
        }
    } else {
        "[↑↓] select  [Enter] agent  [Tab] switch  [q] quit  [?] help"
    };
    let right = format!(
        "tasks: {}  events: {}  {hint}",
        app.tasks.len(),
        app.events.len()
    );

    let line = Line::from(vec![
        Span::styled(left, Style::default().fg(color)),
        Span::raw("  "),
        Span::styled(
            format!(" {focus_txt} "),
            Style::default()
                .fg(Color::Black)
                .bg(focus_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!(" {sound_txt} "),
            Style::default()
                .fg(Color::Black)
                .bg(sound_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
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
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn tab_regions_cover_every_tab() {
        let regions = tab_click_regions(0, 0);
        assert_eq!(regions.len(), Tab::ALL.len());
        for (i, (tab, start, end)) in regions.iter().enumerate() {
            assert_eq!(*tab, Tab::ALL[i]);
            assert!(end > start);
            assert_eq!((end - start) as usize, tab.label().chars().count());
        }
    }

    #[test]
    fn all_tabs_render_without_panic() {
        for tab in Tab::ALL {
            let mut app = App::new();
            app.tab = tab;
            let backend = TestBackend::new(80, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|f| draw(f, &mut app)).unwrap();
        }
    }

    fn render_text(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buffer = terminal.backend().buffer();
        let area = *buffer.area();
        let mut text = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                if let Some(cell) = buffer.cell((x, y)) {
                    text.push_str(cell.symbol());
                }
            }
        }
        text
    }

    #[test]
    fn help_overlay_renders_shortcuts() {
        let mut app = App::new();
        app.popup = Popup::Help { scroll: 0 };
        let text = render_text(&mut app, 100, 40);
        assert!(text.contains("Help"), "help title missing: {text:?}");
        assert!(text.contains("Global"), "global section missing: {text:?}");
        assert!(text.contains("Ctrl+P"), "Ctrl+P row missing: {text:?}");
    }

    #[test]
    fn help_overlay_lists_sound_mute() {
        let mut app = App::new();
        app.popup = Popup::Help { scroll: 0 };
        let text = render_text(&mut app, 100, 40);
        assert!(
            text.contains("mute/unmute sound"),
            "sound help row missing: {text:?}"
        );
    }

    #[test]
    fn status_shows_muted_sound_badge() {
        let mut app = App::new();
        app.sound_enabled = true;
        app.sound_muted = true;
        let text = render_text(&mut app, 140, 24);
        assert!(text.contains("muted"), "mute badge missing: {text:?}");

        app.sound_muted = false;
        let text = render_text(&mut app, 140, 24);
        assert!(!text.contains("muted"), "stale mute badge: {text:?}");
    }

    #[test]
    fn help_overlay_renders_on_small_terminal() {
        let mut app = App::new();
        app.popup = Popup::Help { scroll: 0 };
        // Must not panic when the overlay is larger than the terminal.
        let _ = render_text(&mut app, 20, 6);
    }

    #[test]
    fn help_overlay_scrolls_to_later_content() {
        let mut app = App::new();
        app.popup = Popup::Help { scroll: u16::MAX };
        let text = render_text(&mut app, 100, 12);
        assert!(text.contains("Mouse"), "late section missing: {text:?}");
        assert!(
            !text.contains("Global"),
            "early section still shown: {text:?}"
        );
    }

    #[test]
    fn agent_tab_renders_terminal_screen() {
        let mut app = App::new();
        app.tab = Tab::Agent;
        app.agent_session_id = Some("s1".into());
        app.agent_name = Some("demo".into());
        app.agent_running = true;
        app.term.process(b"hello agent");

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();
        let area = *buffer.area();
        let mut text = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                if let Some(cell) = buffer.cell((x, y)) {
                    text.push_str(cell.symbol());
                }
            }
        }
        assert!(
            text.contains("hello agent"),
            "screen not rendered: {text:?}"
        );
    }

    #[test]
    fn catalog_preview_renders_task_markdown() {
        use super::super::app::CatalogEntry;

        let mut app = App::new();
        app.tab = Tab::Catalog;
        app.catalog = vec![CatalogEntry {
            name: "demo".to_string(),
            agent: Some("opencode".to_string()),
            provider: None,
            model: Some("deepseek-v4-flash".to_string()),
            cwd: None,
            needs: None,
            vars: Vec::new(),
            prompt: String::new(),
        }];
        app.catalog_preview = Some((
            "demo".to_string(),
            "agent = \"opencode\"\n---\n# Heading\n\n- item\n".to_string(),
        ));

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();
        let area = *buffer.area();
        let mut text = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                if let Some(cell) = buffer.cell((x, y)) {
                    text.push_str(cell.symbol());
                }
            }
        }
        assert!(text.contains("Preview"), "preview title missing: {text:?}");
        assert!(
            text.contains("Heading"),
            "rendered heading missing: {text:?}"
        );
        assert!(text.contains("item"), "rendered list missing: {text:?}");
    }

    #[test]
    fn catalog_preview_scrolls_to_later_content() {
        use super::super::app::CatalogEntry;

        let mut app = App::new();
        app.tab = Tab::Catalog;
        app.catalog = vec![CatalogEntry {
            name: "demo".to_string(),
            agent: None,
            provider: None,
            model: None,
            cwd: None,
            needs: None,
            vars: Vec::new(),
            prompt: String::new(),
        }];
        let mut markdown = String::new();
        for i in 0..80 {
            markdown.push_str(&format!("line {i}\n"));
        }
        app.catalog_preview = Some(("demo".to_string(), markdown));

        let backend = TestBackend::new(100, 15);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert!(
            app.catalog_preview_max_scroll > 0,
            "content should overflow the pane"
        );

        app.catalog_preview_scroll = app.catalog_preview_max_scroll;
        terminal.draw(|f| draw(f, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();
        let area = *buffer.area();
        let mut text = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                if let Some(cell) = buffer.cell((x, y)) {
                    text.push_str(cell.symbol());
                }
            }
        }
        assert!(
            text.contains("line 79"),
            "scrolled content missing: {text:?}"
        );
    }

    #[test]
    fn events_tab_renders_payload_panel() {
        use chrono::Utc;
        use favetto_core::model::{Event, EventKind};

        let mut app = App::new();
        app.tab = Tab::Events;
        app.ingest_event(Event {
            id: 1,
            kind: EventKind::Synthetic,
            payload: serde_json::json!({ "hello": "world", "n": 3 }),
            created_at: Utc::now(),
        });

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();
        let area = *buffer.area();
        let mut text = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                if let Some(cell) = buffer.cell((x, y)) {
                    text.push_str(cell.symbol());
                }
            }
        }
        assert!(text.contains("Payload"), "payload title missing: {text:?}");
        assert!(text.contains("hello"), "payload key missing: {text:?}");
        assert!(text.contains("world"), "payload value missing: {text:?}");
    }

    fn geom_task(name: &str) -> favetto_core::model::Task {
        use chrono::Utc;
        use favetto_core::model::TaskStatus;
        favetto_core::model::Task {
            id: uuid::Uuid::new_v4(),
            name: name.to_string(),
            status: TaskStatus::Pending,
            input: serde_json::json!({}),
            output: None,
            dedupe_key: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            error: None,
            session_id: None,
        }
    }

    #[test]
    fn drawn_tasks_record_geometry_and_offset() {
        let mut app = App::new();
        app.tab = Tab::Tasks;
        app.tasks = (0..100).map(|i| geom_task(&format!("t{i}"))).collect();
        app.tasks_selected = 50;

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();

        let geom = app.tasks_geom;
        assert_eq!(geom.len, 100);
        assert!(geom.offset > 0, "the window should scroll to the selection");
        assert!(geom.inner.y > 0, "the rows area sits below the header");

        // Round-trip: the selected data row's screen row maps back to it.
        let screen_row = geom.inner.y + (50 - geom.offset) as u16;
        assert_eq!(geom.row_at(screen_row, geom.inner.x), Some(50));
    }

    #[test]
    fn drawn_schedules_and_notifications_record_geometry() {
        use chrono::Utc;
        use favetto_core::model::{NotificationRecord, Schedule};

        let mut app = App::new();
        app.tab = Tab::Scheduler;
        app.schedules = vec![Schedule {
            id: "s1".to_string(),
            cron: "* * * * *".to_string(),
            task: "t".to_string(),
            input: serde_json::json!({}),
            enabled: true,
            last_run: None,
        }];
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert_eq!(app.schedules_geom.len, 1);
        assert_eq!(app.schedules_geom.offset, 0);
        assert!(
            app.schedules_geom
                .row_at(app.schedules_geom.inner.y, app.schedules_geom.inner.x)
                == Some(0)
        );

        app.tab = Tab::Notifications;
        app.notifications = vec![NotificationRecord {
            id: 1,
            channel: "cli".to_string(),
            subject: "s".to_string(),
            body: "b".to_string(),
            status: "ok".to_string(),
            sent_at: Utc::now(),
        }];
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert_eq!(app.notifications_geom.len, 1);
        assert_eq!(app.notifications_geom.offset, 0);
    }

    #[test]
    fn task_vars_popup_renders_prompts_and_task_name() {
        use crate::tasks::{TaskVar, VarType};

        let mut app = App::new();
        app.popup = Popup::TaskVars(TaskVarsForm {
            task: "open_github_issue".to_string(),
            vars: vec![TaskVar {
                name: "issue_description".to_string(),
                prompt: "Describe the issue".to_string(),
                default: Some("placeholder".to_string()),
                required: true,
                multiline: true,
                var_type: VarType::String,
                choices: None,
            }],
            current: 0,
            values: vec!["placeholder".to_string()],
            choice_selected: vec![0],
            error: None,
        });
        let text = render_text(&mut app, 100, 30);
        assert!(text.contains("Task input"), "title missing: {text:?}");
        assert!(
            text.contains("open_github_issue"),
            "task name missing: {text:?}"
        );
        assert!(
            text.contains("Describe the issue"),
            "prompt missing: {text:?}"
        );
        assert!(text.contains("placeholder"), "default missing: {text:?}");
    }
}
