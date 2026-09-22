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
    table_rows_area, App, CatalogRow, ClickAction, ClickRegion, ConnState, Form, ListGeometry,
    Popup, Tab, TaskVarsForm, Wizard, WizardStep, MENU_OPTIONS,
};
use super::theme::Theme;
use super::{json, markdown};

pub fn draw(frame: &mut Frame, app: &mut App) {
    app.click_regions.clear();

    // Paint the themed base surface across the whole frame before any widget.
    let frame_area = frame.area();
    frame.buffer_mut().set_style(frame_area, app.theme.base());
    let theme = app.theme;

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
        .style(theme.base())
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(theme.block(false)),
        )
        .highlight_style(theme.selected().add_modifier(Modifier::BOLD));
    frame.render_widget(tabs, chunks[0]);

    let tab = app.tab;
    match tab {
        Tab::Tasks => draw_tasks(frame, app, chunks[1], theme),
        Tab::Catalog => draw_catalog(frame, app, chunks[1], theme),
        Tab::Agent => draw_agent(frame, app, chunks[1], theme),
        Tab::Events => draw_events(frame, app, chunks[1], theme),
        Tab::Scheduler => draw_schedules(frame, app, chunks[1], theme),
        Tab::Notifications => draw_notifications(frame, app, chunks[1], theme),
    }

    draw_status(frame, app, chunks[2], theme);

    if !matches!(app.popup, Popup::None) {
        draw_popup(frame, app);
    }
}

fn draw_popup(frame: &mut Frame, app: &mut App) {
    // Copy the theme and clone the throbber state before borrowing `popup`, so
    // the wizard can render a spinner without holding a borrow of `app`.
    let theme = app.theme;
    let state = app.throbber_state.clone();
    let workflow_dot = app.workflow_dot.clone();
    let workflow_path = app.workflow_path.clone();
    let workflow_lines = app.workflow_lines.clone();
    let workflow_note = app.workflow_note.clone();
    match &mut app.popup {
        Popup::None => {}
        Popup::Menu { selected } => draw_menu(frame, *selected, theme),
        Popup::Form(form) => draw_form(frame, form, theme),
        Popup::Wizard(wizard) => draw_wizard(frame, wizard, theme, &state),
        Popup::TaskVars(form) => draw_task_vars(frame, form, theme),
        Popup::Help { scroll } => draw_help(frame, scroll, theme),
        Popup::Workflow { scroll, hscroll } => draw_workflow(
            frame,
            scroll,
            hscroll,
            workflow_lines.as_deref(),
            workflow_note.as_deref(),
            workflow_dot.as_deref(),
            workflow_path.as_deref(),
            theme,
        ),
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

fn draw_menu(frame: &mut Frame, selected: usize, theme: Theme) {
    let area = frame.area();
    let rect = centered_rect(area, 60, MENU_OPTIONS.len() as u16 + 2);

    let items: Vec<ListItem> = MENU_OPTIONS
        .iter()
        .enumerate()
        .map(|(i, option)| {
            let style = if i == selected {
                theme.selected()
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
                .title(" Ctrl+P — New ")
                .style(theme.surface_style())
                .border_style(theme.block(true)),
        )
        .highlight_style(Style::default().add_modifier(Modifier::BOLD));
    frame.render_widget(Clear, rect);
    frame.buffer_mut().set_style(rect, theme.surface_style());
    frame.render_widget(list, rect);
}

fn draw_form(frame: &mut Frame, form: &Form, theme: Theme) {
    let area = frame.area();
    let rect = centered_rect(area, 70, form.fields.len() as u16 + 4);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(form.title, theme.title())));
    lines.push(Line::from(""));
    for (i, field) in form.fields.iter().enumerate() {
        if i < form.current {
            lines.push(Line::from(Span::styled(
                format!("  ✓ {field}: {}", form.values[i]),
                Style::default().fg(theme.success),
            )));
        } else if i == form.current {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("> {field}: "),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(form.input.as_str()),
                Span::styled("█", Style::default().fg(theme.accent)),
            ]));
        } else {
            lines.push(Line::from(format!("  {field}")));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Enter: next · Esc: cancel ",
        Style::default().fg(theme.muted),
    )));

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Step-by-step form ")
                .style(theme.surface_style())
                .border_style(theme.block(true)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(Clear, rect);
    frame.buffer_mut().set_style(rect, theme.surface_style());
    frame.render_widget(paragraph, rect);
}

/// Number of list rows the wizard shows at once.
const WIZARD_VISIBLE: usize = 12;

fn draw_wizard(
    frame: &mut Frame,
    wizard: &Wizard,
    theme: Theme,
    state: &throbber_widgets_tui::ThrobberState,
) {
    let area = frame.area();
    let rows = wizard.choices.len().min(WIZARD_VISIBLE) as u16;
    let rect = centered_rect(area, 70, rows + 6);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(wizard.step.title(), theme.title())));
    lines.push(Line::from(""));

    if wizard.step == WizardStep::Dir {
        lines.push(Line::from(vec![
            Span::styled("> Directory: ", Style::default().fg(theme.accent)),
            Span::raw(wizard.dir.as_str()),
            Span::styled("█", Style::default().fg(theme.accent)),
        ]));
    } else if wizard.loading {
        lines.push(Line::from(vec![
            throbber_span(theme, state),
            Span::styled("loading…", Style::default().fg(theme.muted)),
        ]));
    } else {
        let start = wizard.selected.saturating_sub(WIZARD_VISIBLE - 1);
        for (idx, (label, value)) in wizard
            .choices
            .iter()
            .enumerate()
            .skip(start)
            .take(WIZARD_VISIBLE)
        {
            let unavailable =
                wizard.step == WizardStep::Agent && wizard.available.get(value) == Some(&false);
            let label = if unavailable {
                format!("{label} (not installed)")
            } else {
                label.clone()
            };
            let style = if unavailable {
                Style::default().fg(theme.muted).add_modifier(Modifier::DIM)
            } else if idx == wizard.selected {
                theme.selected()
            } else {
                Style::default()
            };
            lines.push(Line::from(Span::styled(format!(" {label} "), style)));
        }
        if wizard.choices.is_empty() {
            lines.push(Line::from(Span::styled(
                "(nothing available)",
                Style::default().fg(theme.muted),
            )));
        }
    }

    if let Some(err) = &wizard.error {
        lines.push(Line::from(Span::styled(
            err.as_str(),
            Style::default().fg(theme.danger),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " ↑/↓ select · Enter: next · Esc: cancel ",
        Style::default().fg(theme.muted),
    )));

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" New one-shot task ")
                .style(theme.surface_style())
                .border_style(theme.block(true)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(Clear, rect);
    frame.buffer_mut().set_style(rect, theme.surface_style());
    frame.render_widget(paragraph, rect);
}

fn draw_task_vars(frame: &mut Frame, form: &TaskVarsForm, theme: Theme) {
    let area = frame.area();
    let rect = centered_rect(area, 70, form.vars.len() as u16 + 6);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!(" Task input · {} ", form.task),
        theme.title(),
    )));
    lines.push(Line::from(""));

    for (i, var) in form.vars.iter().enumerate() {
        let current = i == form.current;
        let label = format!("{}{}: ", if current { "> " } else { "  " }, var.prompt);
        let label_style = if current {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        if let Some(choices) = &var.choices {
            lines.push(Line::from(Span::styled(label, label_style)));
            let selected = form.choice_selected.get(i).copied().unwrap_or(0);
            for (j, choice) in choices.iter().enumerate() {
                let style = if j == selected {
                    theme.selected()
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
                spans.push(Span::styled("(empty)", Style::default().fg(theme.muted)));
            } else {
                spans.push(Span::raw((*segment).to_string()));
            }
            if current && k == last_segment {
                spans.push(Span::styled("█", Style::default().fg(theme.accent)));
            }
            lines.push(Line::from(spans));
        }
    }

    if let Some(err) = &form.error {
        lines.push(Line::from(Span::styled(
            err.as_str(),
            Style::default().fg(theme.danger),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Tab/↑↓ move · Enter next · Ctrl+Enter submit · Esc cancel ",
        Style::default().fg(theme.muted),
    )));

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Task input ")
                .style(theme.surface_style())
                .border_style(theme.block(true)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(Clear, rect);
    frame.buffer_mut().set_style(rect, theme.surface_style());
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
            ("w", "open/close the workflow graph"),
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
            ("Enter", "start the selected task; on a folder, fold/unfold"),
            ("e", "edit the selected task in your editor"),
            ("Space", "fold/unfold the selected folder"),
            ("p", "show/hide the preview pane"),
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
        "Workflow overlay (w)",
        &[
            ("↑ / ↓ / PageUp / PageDown", "scroll the graph source"),
            ("Esc / w", "close"),
        ],
    ),
    (
        "Mouse",
        &[
            ("click the tab bar", "switch tabs"),
            ("click a catalog folder", "fold/unfold it"),
            ("click a catalog/task row", "select; click again starts it"),
            ("(Agent) wheel/click", "forwarded when the agent enables it"),
        ],
    ),
];

/// Flatten [`HELP_SECTIONS`] into the styled lines rendered by [`draw_help`].
fn help_lines(theme: Theme) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    for (section, rows) in HELP_SECTIONS {
        lines.push(Line::from(Span::styled(
            (*section).to_string(),
            theme.title(),
        )));
        for (k, v) in *rows {
            lines.push(Line::from(vec![
                Span::styled(format!("  {k} — "), Style::default().fg(theme.accent)),
                Span::raw((*v).to_string()),
            ]));
        }
        lines.push(Line::from(""));
    }
    lines
}

/// Draw the scrollable `?` keybinding overlay, clamping `scroll` to the content.
fn draw_help(frame: &mut Frame, scroll: &mut u16, theme: Theme) {
    let area = frame.area();
    let rect = centered_rect(area, 80, area.height.saturating_sub(2));

    let paragraph = Paragraph::new(help_lines(theme))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Help (?) — ↑/↓ scroll, Esc/? close ")
                .style(theme.surface_style())
                .border_style(theme.block(true)),
        )
        .wrap(Wrap { trim: false });
    let inner_width = rect.width.saturating_sub(2);
    let inner_height = rect.height.saturating_sub(2);
    // `line_count` returns `usize`; the preview pane uses the same conversion.
    let max_scroll = (paragraph.line_count(inner_width) as u16).saturating_sub(inner_height);
    *scroll = (*scroll).min(max_scroll);

    frame.render_widget(Clear, rect);
    frame.buffer_mut().set_style(rect, theme.surface_style());
    frame.render_widget(paragraph.scroll((*scroll, 0)), rect);
}

/// Draw the floating workflow overlay. When the structured graph was rendered,
/// shows the cached box-drawing rows and scrolls both axes; otherwise falls back
/// to the wrapped raw DOT (with the reason as a leading note line).
#[allow(clippy::too_many_arguments)]
fn draw_workflow(
    frame: &mut Frame,
    scroll: &mut u16,
    hscroll: &mut u16,
    lines: Option<&[String]>,
    note: Option<&str>,
    dot: Option<&str>,
    path: Option<&str>,
    theme: Theme,
) {
    let area = frame.area();
    let rect = centered_rect(area, 90, area.height.saturating_sub(2));
    let hint = " Workflow (w) — ↑/↓/←/→/PgUp/PgDn scroll, Esc/w close ";
    let title = match path {
        Some(p) => format!("{hint}— {p} "),
        None => hint.to_string(),
    };
    let inner_width = rect.width.saturating_sub(2);
    let inner_height = rect.height.saturating_sub(2);

    frame.render_widget(Clear, rect);
    frame.buffer_mut().set_style(rect, theme.surface_style());

    if let Some(lines) = lines {
        // Cached render: keep the art intact (no wrapping) and scroll both axes.
        let mut content: Vec<Line> = Vec::with_capacity(lines.len() + 1);
        if let Some(note) = note {
            content.push(Line::from(Span::styled(
                note,
                Style::default().fg(theme.muted),
            )));
        }
        content.extend(lines.iter().map(|line| Line::from(line.as_str())));
        let max_line = content.iter().map(Line::width).max().unwrap_or(0) as u16;
        let max_scroll = (content.len() as u16).saturating_sub(inner_height);
        let max_hscroll = max_line.saturating_sub(inner_width);
        *scroll = (*scroll).min(max_scroll);
        *hscroll = (*hscroll).min(max_hscroll);

        let paragraph = Paragraph::new(content)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .style(theme.surface_style())
                    .border_style(theme.block(true)),
            )
            .scroll((*scroll, *hscroll));
        frame.render_widget(paragraph, rect);
        return;
    }

    // Fallback: raw DOT (wrapped), preceded by the failure reason when present.
    let mut content: Vec<Line> = Vec::new();
    if let Some(note) = note {
        content.push(Line::from(Span::styled(
            note,
            Style::default().fg(theme.muted),
        )));
    }
    content.push(Line::from(dot.unwrap_or("Loading workflow graph…")));
    let paragraph = Paragraph::new(content)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .style(theme.surface_style())
                .border_style(theme.block(true)),
        )
        .wrap(Wrap { trim: false });
    let max_scroll = (paragraph.line_count(inner_width) as u16).saturating_sub(inner_height);
    *scroll = (*scroll).min(max_scroll);

    frame.render_widget(paragraph.scroll((*scroll, 0)), rect);
}

/// Minimum Catalog content width at which the tree and preview are docked
/// side-by-side. Below this the preview is drawn as a centered floating overlay
/// so neither pane becomes unusably cramped.
const CATALOG_PREVIEW_DOCK_MIN_WIDTH: u16 = 100;

fn draw_catalog(frame: &mut Frame, app: &mut App, area: Rect, theme: Theme) {
    // Hidden preview: the tree owns the full width and the pane rect is dropped
    // so wheel/PageUp/PageDown stop targeting it.
    if !app.catalog_preview_visible {
        app.catalog_preview_area = None;
        draw_catalog_table(frame, app, area, theme);
        return;
    }

    // Too narrow to dock: keep the tree full-width and float the preview above
    // it. The overlay stays scrollable but does not own the keyboard, so the
    // tree remains navigable behind it.
    if area.width < CATALOG_PREVIEW_DOCK_MIN_WIDTH {
        draw_catalog_table(frame, app, area, theme);
        let rect = centered_rect(area, 80, area.height.saturating_sub(2));
        frame.render_widget(Clear, rect);
        frame.buffer_mut().set_style(rect, theme.surface_style());
        draw_catalog_preview(frame, app, rect, theme);
        return;
    }

    // Wide enough: an even 50/50 split.
    let chunks =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area);
    draw_catalog_table(frame, app, chunks[0], theme);
    draw_catalog_preview(frame, app, chunks[1], theme);
}

/// Folder/task glyphs for the Catalog tree. Terminals that render the emoji at
/// double width and misalign the columns can set `FAVETTO_PLAIN_ICONS=1` for an
/// ASCII fallback.
const FOLDER_OPEN: &str = "📂";
const FOLDER_CLOSED: &str = "📁";
const TASK_ICON: &str = "📄";

/// Whether the ASCII icon fallback is requested via `FAVETTO_PLAIN_ICONS`.
fn plain_icons() -> bool {
    std::env::var("FAVETTO_PLAIN_ICONS")
        .map(|v| v != "0")
        .unwrap_or(false)
}

fn folder_icon(collapsed: bool, plain: bool) -> &'static str {
    match (plain, collapsed) {
        (true, true) => "[+]",
        (true, false) => "[-]",
        (false, true) => FOLDER_CLOSED,
        (false, false) => FOLDER_OPEN,
    }
}

fn task_icon(plain: bool) -> &'static str {
    if plain {
        "-"
    } else {
        TASK_ICON
    }
}

fn draw_catalog_table(frame: &mut Frame, app: &mut App, area: Rect, theme: Theme) {
    let widths = [
        Constraint::Min(14),
        Constraint::Length(10),
        Constraint::Length(18),
        Constraint::Min(0),
    ];
    let header = Row::new(vec!["NAME", "AGENT", "MODEL", "NEEDS"]).style(theme.table_header());
    let plain = plain_icons();

    let rows: Vec<Row> = app
        .catalog_rows()
        .into_iter()
        .map(|row| match row {
            CatalogRow::Folder {
                path,
                depth,
                collapsed,
            } => {
                let label = path.rsplit('/').next().unwrap_or(path.as_str());
                let name = format!(
                    "{} {} {}",
                    "  ".repeat(depth),
                    folder_icon(collapsed, plain),
                    label
                );
                let style = if collapsed {
                    theme.muted_style()
                } else {
                    theme.accent_style().add_modifier(Modifier::BOLD)
                };
                Row::new(vec![
                    Cell::from(name),
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(""),
                ])
                .style(style)
            }
            CatalogRow::Task { index, depth } => {
                let e = &app.catalog[index];
                let name = format!("{} {} {}", "  ".repeat(depth), task_icon(plain), e.stem());
                Row::new(vec![
                    Cell::from(name),
                    Cell::from(e.agent.clone().unwrap_or_else(|| "—".to_string())),
                    Cell::from(e.model_display()),
                    Cell::from(e.needs.clone().unwrap_or_default()),
                ])
            }
        })
        .collect();

    let visible = app.catalog_rows().len();
    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(
                    " Catalog ({}) — Enter/click start · e edit · Space fold ",
                    app.catalog.len()
                ))
                .border_style(theme.block(false)),
        )
        .row_highlight_style(theme.selected())
        .column_spacing(1);

    let mut state = TableState::default();
    state.select(Some(app.catalog_selected));
    frame.render_stateful_widget(table, area, &mut state);
    app.catalog_geom = ListGeometry {
        inner: table_rows_area(area),
        offset: state.offset(),
        len: visible,
    };
}

fn draw_catalog_preview(frame: &mut Frame, app: &mut App, area: Rect, theme: Theme) {
    // Remember the pane's rectangle so the mouse wheel can be targeted at it.
    app.catalog_preview_area = Some(area);

    let (name, lines) = match app.catalog_preview.as_ref() {
        Some((name, markdown)) if !markdown.trim().is_empty() => {
            (Some(name.as_str()), markdown::render_task(markdown, &theme))
        }
        Some((name, _)) => (
            Some(name.as_str()),
            vec![Line::from(Span::styled(
                "(no content)",
                Style::default().fg(theme.muted),
            ))],
        ),
        None => (
            None,
            vec![Line::from(Span::styled(
                "Select a task to preview its definition.",
                Style::default().fg(theme.muted),
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
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(theme.block(false)),
        )
        .scroll((app.catalog_preview_scroll, 0));
    frame.render_widget(paragraph, area);
}

fn draw_schedules(frame: &mut Frame, app: &mut App, area: Rect, theme: Theme) {
    let widths = [
        Constraint::Length(10),
        Constraint::Length(20),
        Constraint::Length(26),
        Constraint::Length(12),
        Constraint::Min(0),
    ];
    let header =
        Row::new(vec!["ID", "CRON", "TASK", "ENABLED", "INPUT"]).style(theme.table_header());

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
                .title(format!(
                    " Schedules ({}) — click or ↑/↓ select ",
                    app.schedules.len()
                ))
                .border_style(theme.block(false)),
        )
        .row_highlight_style(theme.selected())
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

fn draw_notifications(frame: &mut Frame, app: &mut App, area: Rect, theme: Theme) {
    let widths = [
        Constraint::Length(8),
        Constraint::Length(12),
        Constraint::Length(10),
        Constraint::Length(28),
        Constraint::Min(0),
    ];
    let header =
        Row::new(vec!["ID", "CHANNEL", "STATUS", "SUBJECT", "BODY"]).style(theme.table_header());

    let rows: Vec<Row> = app
        .notifications
        .iter()
        .map(|n| {
            let body = n.body.chars().take(40).collect::<String>();
            Row::new(vec![
                Cell::from(n.id.to_string()),
                Cell::from(n.channel.clone()),
                Cell::from(status_ok(&n.status, theme)),
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
                .title(format!(
                    " Notifications ({}) — click or ↑/↓ select ",
                    app.notifications.len()
                ))
                .border_style(theme.block(false)),
        )
        .row_highlight_style(theme.selected())
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

fn status_ok(s: &str, theme: Theme) -> Line<'static> {
    let (dot, txt, color) = if s == "ok" {
        ("●", "ok", theme.success)
    } else {
        ("○", "error", theme.danger)
    };
    Line::from(vec![
        Span::styled(format!("{dot} "), Style::default().fg(color)),
        Span::styled(txt.to_string(), Style::default().fg(color)),
    ])
}

fn draw_tasks(frame: &mut Frame, app: &mut App, area: Rect, theme: Theme) {
    let widths = [
        Constraint::Length(10),
        Constraint::Length(22),
        Constraint::Length(30),
        Constraint::Length(12),
        Constraint::Length(8),
        Constraint::Min(0),
    ];
    let header = Row::new(vec!["ID", "TASK", "SESSION", "STATUS", "AGE", "ERROR"])
        .style(theme.table_header());

    let state = app.throbber_state.clone();
    let rows: Vec<Row> = app
        .tasks
        .iter()
        .map(|t| {
            Row::new(vec![
                Cell::from(short_id(&t.id)),
                Cell::from(t.name.clone()),
                Cell::from(truncate_ellipsis(
                    t.session_title.as_deref().unwrap_or(""),
                    28,
                )),
                Cell::from(status_line(t.status, theme, &state)),
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
                    " Tasks ({}) — Enter/click to run or answer an agent ",
                    app.tasks.len()
                ))
                .border_style(theme.block(false)),
        )
        .row_highlight_style(theme.selected())
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

fn draw_agent(frame: &mut Frame, app: &mut App, area: Rect, theme: Theme) {
    let inner = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.block(false));
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

    let title: Line = if app.agent_session_id.is_none() {
        Line::from(Span::raw(" Agent — no session (Enter on a task) "))
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
        let mut spans = vec![Span::raw(" Agent · ")];
        if app.agent_running {
            spans.push(throbber_span(theme, &app.throbber_state));
        }
        spans.push(Span::raw(format!(
            "{name} · {state}{extra} · Ctrl+N new · Ctrl+Q leave "
        )));
        Line::from(spans)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.block(false))
        .title(title);
    frame.render_widget(block, area);

    if app.agent_session_id.is_none() || inner_area.width == 0 || inner_area.height == 0 {
        let hint = Paragraph::new(Line::from(Span::styled(
            "The embedded agent terminal appears here. Select a task and press Enter to launch the configured agent.",
            Style::default().fg(theme.muted),
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
            let style = cell_style(cell, theme);
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

/// Map a vt100 cell's attributes to a ratatui style, defaulting unset colours to
/// the active theme so the embedded agent adopts the themed surface.
fn cell_style(cell: &vt100::Cell, theme: Theme) -> Style {
    let mut style = Style::default();
    let (mut fg, mut bg) = (
        vt_color_or(cell.fgcolor(), theme.fg),
        vt_color_or(cell.bgcolor(), theme.bg),
    );

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

/// Convert a vt100 colour to a ratatui colour, substituting the theme default.
fn vt_color_or(color: vt100::Color, default: Color) -> Color {
    match color {
        vt100::Color::Default => default,
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

fn draw_events(frame: &mut Frame, app: &mut App, area: Rect, theme: Theme) {
    let chunks =
        Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).split(area);
    draw_event_list(frame, app, chunks[0], theme);
    draw_event_payload(frame, app, chunks[1], theme);
}

fn draw_event_list(frame: &mut Frame, app: &mut App, area: Rect, theme: Theme) {
    let widths = [
        Constraint::Length(8),
        Constraint::Min(18),
        Constraint::Length(8),
    ];
    let header = Row::new(vec!["ID", "KIND", "AGE"]).style(theme.table_header());

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
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(
                    " Events ({}) — click or ↑/↓ select ",
                    app.events.len()
                ))
                .border_style(theme.block(false)),
        )
        .row_highlight_style(theme.selected())
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

fn draw_event_payload(frame: &mut Frame, app: &App, area: Rect, theme: Theme) {
    let (title, lines) = match app.selected_event() {
        Some(ev) => (
            format!(" Payload · {} #{} ", ev.kind.as_str(), ev.id),
            json::render(&ev.payload, &theme),
        ),
        None => (
            " Payload ".to_string(),
            vec![Line::from(Span::styled(
                "(no events yet)",
                Style::default().fg(theme.muted),
            ))],
        ),
    };

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(theme.block(false)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect, theme: Theme) {
    let (dot, state_txt, color) = match app.conn {
        ConnState::Connected => ("●", "connected", theme.success),
        ConnState::Connecting => ("◐", "connecting", theme.warning),
        ConnState::Disconnected => ("○", "disconnected", theme.danger),
    };

    // Keyboard focus: the embedded agent only owns the keyboard while its tab is
    // open and capture is on; otherwise favetto does.
    let agent_focused = app.tab == Tab::Agent && app.agent_capture;
    let (focus_txt, focus_color) = if agent_focused {
        ("agent", theme.info)
    } else {
        ("favetto", theme.accent)
    };

    let hint = if app.tab == Tab::Agent {
        if agent_focused {
            "[Ctrl+Y] favetto keys"
        } else {
            "[Ctrl+Y] agent keys  [Ctrl+Q] leave  [Ctrl+N] new  [?] help"
        }
    } else {
        "[↑↓] select  [Enter] agent  [Tab] switch  [q] quit  [?] help"
    };

    // Sound badge: muted wins visually; a disabled engine shows `sound off`.
    let (sound_txt, sound_color) = if !app.sound_enabled {
        ("sound off", theme.muted)
    } else if app.sound_muted {
        ("muted", theme.warning)
    } else {
        ("sound", theme.success)
    };

    let mut spans = vec![
        Span::raw(" "),
        Span::styled(format!(" {dot} "), Style::default().fg(color)),
        Span::styled(state_txt.to_string(), Style::default().fg(color)),
    ];
    if matches!(app.conn, ConnState::Connecting) {
        spans.push(throbber_span(theme, &app.throbber_state));
    }
    spans.push(Span::styled(
        format!("· {}", app.conn_detail),
        Style::default().fg(theme.muted),
    ));

    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        format!(" {sound_txt} "),
        Style::default()
            .fg(theme.selected_fg)
            .bg(sound_color)
            .add_modifier(Modifier::BOLD),
    ));

    let awaiting = app
        .tasks
        .iter()
        .filter(|t| t.status == TaskStatus::AwaitingInput)
        .count();
    if awaiting > 0 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("⚠ {awaiting} awaiting input · [Enter] open agent"),
            Style::default()
                .fg(theme.warning)
                .add_modifier(Modifier::BOLD),
        ));
    }

    let counts = format!("tasks: {}  events: {}", app.tasks.len(), app.events.len());
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        format!(" {focus_txt} "),
        Style::default()
            .fg(theme.selected_fg)
            .bg(focus_color)
            .add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::raw("  "));
    spans.push(Span::styled(counts, theme.muted_style()));
    spans.push(Span::styled(format!("  {hint}"), theme.accent_style()));

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// A status glyph/colour line for a task row. `Running` renders a throbber.
fn status_line(
    s: TaskStatus,
    theme: Theme,
    state: &throbber_widgets_tui::ThrobberState,
) -> Line<'static> {
    let color = theme.semantic(s);
    let (glyph, txt) = match s {
        TaskStatus::Pending => ("○", "pending"),
        TaskStatus::Running => ("", "running"),
        TaskStatus::AwaitingInput => ("!", "awaiting"),
        TaskStatus::Succeeded => ("✓", "succeeded"),
        TaskStatus::Failed => ("✗", "failed"),
        TaskStatus::Cancelled => ("●", "cancelled"),
    };
    let mut spans = Vec::new();
    if s == TaskStatus::Running {
        spans.push(throbber_span(theme, state));
    } else {
        spans.push(Span::styled(
            format!("{glyph} "),
            Style::default().fg(color),
        ));
    }
    spans.push(Span::styled(txt.to_string(), Style::default().fg(color)));
    Line::from(spans)
}

/// A styled throbber symbol span for the active theme.
fn throbber_span(theme: Theme, state: &throbber_widgets_tui::ThrobberState) -> Span<'static> {
    let t: throbber_widgets_tui::Throbber<'static> = throbber_widgets_tui::Throbber::default()
        .throbber_set(throbber_widgets_tui::BRAILLE_SIX)
        .throbber_style(Style::default().fg(theme.accent))
        .style(theme.base());
    t.to_symbol_span(state)
}

fn short_id(id: &uuid::Uuid) -> String {
    id.to_string().split('-').next().unwrap_or("?").to_string()
}

fn short_str(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Truncate to `max` chars, appending `…` when shortened.
fn truncate_ellipsis(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    format!(
        "{}…",
        s.chars().take(max.saturating_sub(1)).collect::<String>()
    )
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
    use std::collections::BTreeMap;

    use favetto_core::model::AgentCapabilities;

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
        for theme in [Theme::dark(), Theme::light()] {
            for tab in Tab::ALL {
                let mut app = App::with_theme(theme);
                app.tab = tab;
                let backend = TestBackend::new(80, 24);
                let mut terminal = Terminal::new(backend).unwrap();
                terminal.draw(|f| draw(f, &mut app)).unwrap();
            }
        }
    }

    #[test]
    fn all_popups_render_without_panic() {
        for theme in [Theme::dark(), Theme::light()] {
            for popup in [
                Popup::Menu { selected: 0 },
                Popup::Form(Form {
                    kind: super::super::app::FormKind::AddTask,
                    title: "Add task to catalog",
                    fields: vec!["Task name", "Prompt"],
                    current: 0,
                    values: Vec::new(),
                    input: String::new(),
                }),
                Popup::Wizard(Wizard {
                    step: WizardStep::Agent,
                    selected: 0,
                    choices: vec![("opencode".to_string(), "opencode".to_string())],
                    providers: Vec::new(),
                    capabilities: AgentCapabilities::default(),
                    agent_caps: BTreeMap::new(),
                    available: BTreeMap::new(),
                    agent: None,
                    provider: None,
                    model: None,
                    dir: String::new(),
                    loading: false,
                    error: None,
                }),
                Popup::Wizard(Wizard {
                    step: WizardStep::Provider,
                    selected: 0,
                    choices: Vec::new(),
                    providers: Vec::new(),
                    capabilities: AgentCapabilities::default(),
                    agent_caps: BTreeMap::new(),
                    available: BTreeMap::new(),
                    agent: Some("opencode".to_string()),
                    provider: None,
                    model: None,
                    dir: String::new(),
                    loading: true,
                    error: Some("boom".to_string()),
                }),
                Popup::TaskVars(TaskVarsForm {
                    task: "issue".to_string(),
                    vars: vec![crate::tasks::TaskVar {
                        name: "description".to_string(),
                        prompt: "Describe".to_string(),
                        default: Some("draft".to_string()),
                        required: true,
                        multiline: false,
                        var_type: crate::tasks::VarType::String,
                        choices: None,
                    }],
                    current: 0,
                    values: vec!["draft".to_string()],
                    choice_selected: vec![0],
                    error: None,
                }),
                Popup::Help { scroll: 0 },
                Popup::Workflow {
                    scroll: 0,
                    hscroll: 0,
                },
            ] {
                let mut app = App::with_theme(theme);
                app.popup = popup;
                let backend = TestBackend::new(100, 40);
                let mut terminal = Terminal::new(backend).unwrap();
                terminal.draw(|f| draw(f, &mut app)).unwrap();
            }
        }
    }

    #[test]
    fn wizard_marks_unavailable_agent() {
        let mut app = App::new();
        app.popup = Popup::Wizard(Wizard {
            step: WizardStep::Agent,
            selected: 0,
            choices: vec![
                ("opencode".to_string(), "opencode".to_string()),
                ("claude".to_string(), "claude".to_string()),
            ],
            providers: Vec::new(),
            capabilities: AgentCapabilities::default(),
            agent_caps: BTreeMap::new(),
            available: BTreeMap::from([
                ("opencode".to_string(), true),
                ("claude".to_string(), false),
            ]),
            agent: None,
            provider: None,
            model: None,
            dir: String::new(),
            loading: false,
            error: None,
        });

        let text = render_text(&mut app, 100, 40);
        assert!(
            text.contains("claude (not installed)"),
            "unavailable marker missing: {text:?}"
        );
    }

    #[test]
    fn base_surface_paints_theme_background() {
        let theme = Theme::dark();
        let mut app = App::with_theme(theme);
        app.tab = Tab::Notifications;
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();
        // A cell inside an otherwise-empty notifications table row.
        let cell = buffer.cell((3, 2)).unwrap();
        assert_eq!(cell.bg, theme.bg);
        // Guard against a regression to `Color::Reset` on the base paint.
        assert_ne!(theme.bg, Color::Reset);
    }

    #[test]
    fn status_bar_shows_connection_dot() {
        let mut app = App::new();
        app.conn = ConnState::Connected;
        let text = render_text(&mut app, 100, 24);
        assert!(text.contains('●'), "connected dot missing: {text:?}");

        app.conn = ConnState::Disconnected;
        let text = render_text(&mut app, 100, 24);
        assert!(text.contains('○'), "disconnected dot missing: {text:?}");
    }

    #[test]
    fn running_task_renders_throbber_and_selection_uses_theme() {
        let theme = Theme::dark();
        let mut app = App::with_theme(theme);
        app.tab = Tab::Tasks;
        let mut running = geom_task("r");
        running.status = favetto_core::model::TaskStatus::Running;
        app.tasks = vec![running];

        let text = render_text(&mut app, 100, 24);
        // The `BRAILLE_SIX` throbber set used by `throbber_span`.
        const BRAILLE: [char; 6] = ['⠷', '⠯', '⠟', '⠻', '⠽', '⠾'];
        assert!(
            BRAILLE.iter().any(|c| text.contains(*c)),
            "throbber glyph missing: {text:?}"
        );

        // The throbber span actually changes across animation steps.
        let mut state = throbber_widgets_tui::ThrobberState::default();
        let first = throbber_span(theme, &state).content.to_string();
        state.calc_next();
        let second = throbber_span(theme, &state).content.to_string();
        assert_ne!(first, second);
    }

    #[test]
    fn status_line_shows_awaiting_input() {
        let theme = Theme::dark();
        let state = throbber_widgets_tui::ThrobberState::default();
        let line = status_line(TaskStatus::AwaitingInput, theme, &state);
        let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains('!'), "glyph missing: {text:?}");
        assert!(text.contains("awaiting"), "label missing: {text:?}");
    }

    #[test]
    fn status_bar_counts_awaiting_input_tasks() {
        let mut app = App::new();
        app.tab = Tab::Tasks;
        let mut blocked = geom_task("blocked");
        blocked.status = TaskStatus::AwaitingInput;
        app.tasks = vec![blocked];

        let text = render_text(&mut app, 120, 24);
        assert!(
            text.contains("awaiting input"),
            "status bar missing: {text:?}"
        );
        assert!(text.contains("open agent"), "hint missing: {text:?}");
    }

    #[test]
    fn task_session_title_renders_in_session_column() {
        let mut app = App::new();
        app.tab = Tab::Tasks;
        let mut task = geom_task("implement_favetto_issue");
        task.session_title = Some("Implement issue 31 title column".to_string());
        app.tasks = vec![task];

        let text = render_text(&mut app, 120, 24);
        assert!(text.contains("SESSION"), "header missing: {text:?}");
        assert!(
            text.contains("Implement issue 31"),
            "title missing: {text:?}"
        );
    }

    #[test]
    fn task_without_session_title_renders_blank_cell() {
        let mut app = App::new();
        app.tab = Tab::Tasks;
        app.tasks = vec![geom_task("no_title")];

        // No panic and no stray title text.
        let text = render_text(&mut app, 100, 24);
        assert!(text.contains("SESSION"), "header missing: {text:?}");
    }

    #[test]
    fn truncate_ellipsis_shortens_with_marker() {
        assert_eq!(truncate_ellipsis("short", 28), "short");
        let long = "a".repeat(40);
        let truncated = truncate_ellipsis(&long, 28);
        assert_eq!(truncated.chars().count(), 28);
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn agent_default_colors_adopt_theme() {
        let theme = Theme::dark();
        let mut parser = vt100::Parser::new(4, 20, 0);
        parser.process(b"plain");
        let cell = parser.screen().cell(0, 0).unwrap();
        let style = cell_style(cell, theme);
        assert_eq!(style.fg, Some(theme.fg));
        assert_eq!(style.bg, Some(theme.bg));

        // Explicit colours are preserved, not replaced by the theme default.
        let mut parser = vt100::Parser::new(4, 20, 0);
        parser.process(b"\x1b[31mX");
        let cell = parser.screen().cell(0, 0).unwrap();
        let style = cell_style(cell, theme);
        assert_eq!(style.fg, Some(Color::Indexed(1)));
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
    fn help_overlay_lists_workflow_key() {
        let mut app = App::new();
        app.popup = Popup::Help { scroll: 0 };
        let text = render_text(&mut app, 100, 40);
        assert!(
            text.contains("workflow graph"),
            "workflow help row missing: {text:?}"
        );
    }

    #[test]
    fn help_overlay_lists_editor_key() {
        let mut app = App::new();
        app.popup = Popup::Help { scroll: 0 };
        let text = render_text(&mut app, 100, 40);
        assert!(
            text.contains("edit the selected task"),
            "editor help row missing: {text:?}"
        );
    }

    #[test]
    fn workflow_overlay_renders_graph() {
        let mut app = App::new();
        app.popup = Popup::Workflow {
            scroll: 0,
            hscroll: 0,
        };
        app.workflow_lines = Some(vec![
            "┌────────┐  ┌──────────────┐".to_string(),
            "│ a      │  │ ghost        │".to_string(),
            "└────────┘  └──────────────┘".to_string(),
            "      \"spawn\"   \"needs\"".to_string(),
        ]);
        app.workflow_dot = Some("digraph workflow { }".to_string());
        app.workflow_path = Some("/data/workflow.dot".to_string());
        let text = render_text(&mut app, 100, 40);
        assert!(
            text.contains("Workflow"),
            "workflow title missing: {text:?}"
        );
        assert!(text.contains('┌'), "box drawing missing: {text:?}");
        assert!(text.contains("spawn"), "spawn label missing: {text:?}");
        assert!(text.contains("needs"), "needs label missing: {text:?}");
        assert!(
            !text.contains("digraph"),
            "raw DOT leaked into graph render: {text:?}"
        );
    }

    #[test]
    fn workflow_overlay_falls_back_to_dot() {
        let mut app = App::new();
        app.popup = Popup::Workflow {
            scroll: 0,
            hscroll: 0,
        };
        app.workflow_dot =
            Some("digraph workflow {\n  \"a\" -> \"b\" [label=\"spawn\"];\n}".to_string());
        app.workflow_note = Some("structured graph unavailable; showing raw DOT".to_string());
        let text = render_text(&mut app, 100, 40);
        assert!(
            text.contains("structured graph unavailable"),
            "fallback note missing: {text:?}"
        );
        assert!(
            text.contains("digraph workflow"),
            "dot source missing: {text:?}"
        );
    }

    #[test]
    fn workflow_overlay_renders_loading_state() {
        let mut app = App::new();
        app.popup = Popup::Workflow {
            scroll: 0,
            hscroll: 0,
        };
        let text = render_text(&mut app, 100, 24);
        assert!(
            text.contains("Loading workflow graph"),
            "loading placeholder missing: {text:?}"
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
        assert!(
            text.contains("(Agent) wheel/click"),
            "late section missing: {text:?}"
        );
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
    fn catalog_docks_preview_at_half_width_when_wide() {
        let mut app = App::new();
        app.tab = Tab::Catalog;
        app.catalog = vec![catalog_entry("demo")];
        app.catalog_preview = Some(("demo".to_string(), "# Heading\n".to_string()));

        let _ = render_text(&mut app, 120, 30);
        let area = app.catalog_preview_area.expect("preview pane rect");
        // The content spans the full 120 columns, so a 50/50 split starts at 60.
        assert_eq!(area.x, 60, "preview should start at the midpoint");
        assert_eq!(area.width, 60, "preview should take half the width");
    }

    #[test]
    fn catalog_preview_hidden_takes_no_space() {
        let mut app = App::new();
        app.tab = Tab::Catalog;
        app.set_catalog(vec![catalog_entry("pipelines/plan")]);
        app.catalog_preview = Some(("pipelines/plan".to_string(), "# Heading\n".to_string()));
        app.catalog_preview_visible = false;

        let text = render_text(&mut app, 120, 30);
        assert!(text.contains("Catalog"), "catalog tree missing: {text:?}");
        assert!(
            !text.contains("Preview"),
            "hidden preview should not render: {text:?}"
        );
        assert!(
            app.catalog_preview_area.is_none(),
            "hidden preview must clear its hit-test rect"
        );
    }

    #[test]
    fn catalog_preview_floats_when_narrow() {
        let mut app = App::new();
        app.tab = Tab::Catalog;
        app.set_catalog(vec![catalog_entry("demo")]);
        app.catalog_preview = Some(("demo".to_string(), "# Heading\n".to_string()));

        // Below the dock threshold the tree stays full-width and the preview
        // floats centered above it.
        let text = render_text(&mut app, CATALOG_PREVIEW_DOCK_MIN_WIDTH - 1, 30);
        assert!(text.contains("Catalog"), "catalog tree missing: {text:?}");
        assert!(
            text.contains("Preview"),
            "floating preview missing: {text:?}"
        );
        assert!(
            text.contains("Heading"),
            "preview content missing: {text:?}"
        );
        let area = app.catalog_preview_area.expect("preview pane rect");
        assert!(
            area.width < CATALOG_PREVIEW_DOCK_MIN_WIDTH,
            "floating preview should be inset: {area:?}"
        );
        assert!(
            area.x > 0,
            "floating preview should be centered horizontally: {area:?}"
        );
    }

    /// A catalog entry with just a name (all optional fields unset).
    fn catalog_entry(name: &str) -> super::super::app::CatalogEntry {
        super::super::app::CatalogEntry {
            name: name.to_string(),
            agent: None,
            provider: None,
            model: None,
            cwd: None,
            needs: None,
            vars: Vec::new(),
            prompt: String::new(),
        }
    }

    #[test]
    fn catalog_renders_folder_and_task_icons() {
        let mut app = App::new();
        app.tab = Tab::Catalog;
        app.set_catalog(vec![
            catalog_entry("triage"),
            catalog_entry("pipelines/plan"),
        ]);

        let text = render_text(&mut app, 100, 30);
        assert!(text.contains("Catalog"), "catalog title missing: {text:?}");
        assert!(text.contains("pipelines"), "folder label missing: {text:?}");
        assert!(text.contains("plan"), "task label missing: {text:?}");
        assert!(
            text.contains(FOLDER_OPEN),
            "open-folder icon missing: {text:?}"
        );
        assert!(text.contains(TASK_ICON), "task icon missing: {text:?}");
    }

    #[test]
    fn collapsed_folder_hides_task_in_render() {
        let mut app = App::new();
        app.tab = Tab::Catalog;
        app.set_catalog(vec![catalog_entry("pipelines/plan")]);
        app.catalog_collapsed.insert("pipelines".to_string());

        let text = render_text(&mut app, 100, 30);
        assert!(
            text.contains("pipelines"),
            "collapsed folder label missing: {text:?}"
        );
        assert!(
            text.contains(FOLDER_CLOSED),
            "closed-folder icon missing: {text:?}"
        );
        assert!(
            !text.contains("plan"),
            "collapsed folder should hide its task: {text:?}"
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
            session_title: None,
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
