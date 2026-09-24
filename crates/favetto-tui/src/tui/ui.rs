//! ratatui rendering: header tabs, per-tab content, and a connection status bar.

use chrono::{DateTime, Utc};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table, TableState, Tabs, Wrap,
};
use ratatui::Frame;

use favetto_core::model::{AgentSessionInfo, TaskStatus};

use super::app::{
    activity_cell, format_activity, format_usage, table_rows_area, usage_cell, App, CatalogRow,
    ClickAction, ClickRegion, ConnState, Form, ListGeometry, Popup, ReplyPrompt, Tab, TaskVarsForm,
    Wizard, WizardStep, MENU_OPTIONS,
};
use super::text_buffer::TextBuffer;
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
        Popup::Form(form) => {
            draw_form(frame, form, theme);
        }
        Popup::Wizard(wizard) => {
            draw_wizard(frame, wizard, theme, &state);
        }
        Popup::TaskVars(form) => {
            draw_task_vars(frame, form, theme);
        }
        Popup::Help { scroll } => draw_help(frame, scroll, theme),
        Popup::Workflow { scroll, hscroll } => draw_workflow(
            frame,
            WorkflowOverlay {
                scroll,
                hscroll,
                lines: workflow_lines.as_deref(),
                note: workflow_note.as_deref(),
                dot: workflow_dot.as_deref(),
                path: workflow_path.as_deref(),
                theme,
            },
        ),
        Popup::Sessions { selected, sessions } => {
            draw_sessions_picker(frame, *selected, sessions, theme)
        }
        Popup::Reply(prompt) => draw_reply(frame, prompt, theme),
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

/// Rendered height (rows) of `lines` wrapped at `width`, excluding any block.
fn wrapped_height(lines: &[Line<'_>], width: u16) -> u16 {
    Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: false })
        .line_count(width) as u16
}

/// Wrapped content row (0-based) of the logical line holding the cursor/selection.
fn focus_row(content: &[Line<'_>], focus_line: usize, width: u16) -> u16 {
    let split = focus_line.min(content.len());
    let before = wrapped_height(&content[..split], width);
    let own = if split < content.len() {
        wrapped_height(&content[split..=split], width)
    } else {
        0
    };
    before.saturating_add(own.saturating_sub(1))
}

/// Popup rect sized from the rendered content plus a docked footer, together with
/// the scroll offset that keeps `focus` (a wrapped row) visible.
fn scrolling_body_layout(
    area: Rect,
    width_pct: u16,
    content_height: u16,
    footer_height: u16,
    focus: u16,
) -> (Rect, u16) {
    let desired = content_height
        .saturating_add(footer_height)
        .saturating_add(2); // top + bottom border
    let rect = centered_rect(area, width_pct, desired);
    let inner_height = rect.height.saturating_sub(2);
    let viewport = inner_height.saturating_sub(footer_height);
    let max_scroll = content_height.saturating_sub(viewport);
    let scroll = if viewport == 0 || focus < viewport {
        0
    } else {
        focus
            .saturating_add(1)
            .saturating_sub(viewport)
            .min(max_scroll)
    };
    (rect, scroll)
}

/// Arguments for [`draw_growing_popup`], grouping the popup's content, geometry,
/// and theme so the render helper's signature stays small.
struct GrowingPopup {
    /// Outer frame area the popup is centered in.
    area: Rect,
    /// Popup width as a percentage of `area`.
    width_pct: u16,
    title: String,
    content: Vec<Line<'static>>,
    footer: Vec<Line<'static>>,
    focus_line: usize,
    theme: Theme,
}

/// Render a growing, scrolling popup whose footer stays docked, returning its
/// rect so callers/tests can inspect the geometry.
fn draw_growing_popup(frame: &mut Frame, popup: GrowingPopup) -> Rect {
    let GrowingPopup {
        area,
        width_pct,
        title,
        content,
        footer,
        focus_line,
        theme,
    } = popup;
    let inner_width = centered_rect(area, width_pct, 0).width.saturating_sub(2);
    let content_height = wrapped_height(&content, inner_width);
    let footer_height = wrapped_height(&footer, inner_width); // may wrap too
    let focus = focus_row(&content, focus_line, inner_width);
    let (rect, scroll) =
        scrolling_body_layout(area, width_pct, content_height, footer_height, focus);

    frame.render_widget(Clear, rect);
    frame.buffer_mut().set_style(rect, theme.surface_style());
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(theme.surface_style())
        .border_style(theme.block(true));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    // Content scrolls; footer is a separate, docked row block.
    let chunks =
        Layout::vertical([Constraint::Min(0), Constraint::Length(footer_height)]).split(inner);
    frame.render_widget(
        Paragraph::new(content)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        chunks[0],
    );
    frame.render_widget(Paragraph::new(footer).wrap(Wrap { trim: false }), chunks[1]);
    rect
}

/// A text buffer line split around the caret, for rendering.
struct DisplayLine {
    /// Text before the caret (the whole line when the caret is elsewhere).
    before: String,
    /// Text after the caret (empty when the caret is elsewhere).
    after: String,
    /// Whether the caret is on this line.
    caret: bool,
}

/// Split `buf` into displayable lines, marking the caret's line and the text on
/// each side of the caret within it. Always returns at least one line.
fn display_lines(buf: &TextBuffer) -> Vec<DisplayLine> {
    let value = buf.value();
    let caret = buf.caret();
    let caret_line = value[..caret].matches('\n').count();
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (idx, segment) in value.split('\n').enumerate() {
        if idx == caret_line {
            let col = caret - start;
            lines.push(DisplayLine {
                before: segment[..col].to_string(),
                after: segment[col..].to_string(),
                caret: true,
            });
        } else {
            lines.push(DisplayLine {
                before: segment.to_string(),
                after: String::new(),
                caret: false,
            });
        }
        start += segment.len() + 1;
    }
    lines
}

/// Append the caret-annotated text of a single-line `buf` to `spans`, rendering
/// the caret between the text before and after it. Extra lines are ignored.
fn push_caret_line(spans: &mut Vec<Span<'static>>, buf: &TextBuffer, caret: Style) {
    if let Some(line) = display_lines(buf).into_iter().next() {
        spans.push(Span::raw(line.before));
        if line.caret {
            spans.push(Span::styled("█", caret));
        }
        spans.push(Span::raw(line.after));
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

/// Draw the Ctrl+O session picker: one row per live/retained agent session,
/// labelled with its agent, state, and task, so concurrent runs can be told apart.
fn draw_sessions_picker(
    frame: &mut Frame,
    selected: usize,
    sessions: &[AgentSessionInfo],
    theme: Theme,
) {
    let area = frame.area();
    let rect = centered_rect(area, 70, (sessions.len() as u16).saturating_add(2));

    let items: Vec<ListItem> = sessions
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let style = if i == selected {
                theme.selected()
            } else {
                Style::default()
            };
            let state = match (s.headless, s.running) {
                (true, true) => "headless",
                (true, false) => "headless · finished",
                (false, true) => "running",
                (false, false) => "stopped",
            };
            let task = s
                .task_id
                .as_deref()
                .map(|t| format!(" · task {}", short_str(t)))
                .unwrap_or_default();
            let mut detail = String::new();
            if let Some(activity) = &s.activity {
                detail.push_str(&format!(" · {}", format_activity(activity)));
            }
            if let Some(usage) = &s.usage {
                if !usage.is_empty() {
                    detail.push_str(&format!(" · {}", format_usage(usage)));
                }
            }
            ListItem::new(Line::from(Span::styled(
                format!(" {} · {state}{task}{detail} ", s.agent),
                style,
            )))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Ctrl+O — Agent sessions ")
                .style(theme.surface_style())
                .border_style(theme.block(true)),
        )
        .highlight_style(Style::default().add_modifier(Modifier::BOLD));
    frame.render_widget(Clear, rect);
    frame.buffer_mut().set_style(rect, theme.surface_style());
    frame.render_widget(list, rect);
}

/// Draw the Ctrl+R reply popup: the precise prompt from the agent's state
/// channel, its selectable options (or a free-form input), and the send keys.
fn draw_reply(frame: &mut Frame, prompt: &ReplyPrompt, theme: Theme) {
    let area = frame.area();
    let mut content: Vec<Line<'static>> = Vec::new();
    content.push(Line::from(Span::styled(
        format!("Agent is waiting on {}", short_str(&prompt.session_id)),
        Style::default().fg(theme.muted),
    )));
    content.push(Line::from(""));
    for line in prompt.request.message.lines() {
        content.push(Line::from(line.to_string()));
    }
    content.push(Line::from(""));
    if prompt.has_options() {
        for (i, option) in prompt.request.options.iter().enumerate() {
            let style = if i == prompt.selected {
                theme.selected()
            } else {
                Style::default()
            };
            content.push(Line::from(Span::styled(format!("  {option} "), style)));
        }
    } else {
        let mut spans = vec![Span::styled("> ", Style::default().fg(theme.accent))];
        push_caret_line(&mut spans, &prompt.input, Style::default().fg(theme.accent));
        content.push(Line::from(spans));
    }

    let footer = vec![Line::from(Span::styled(
        if prompt.has_options() {
            "↑/↓ select · Enter send · Esc cancel"
        } else {
            "type answer · Enter send · Esc cancel"
        },
        Style::default().fg(theme.muted),
    ))];

    draw_growing_popup(
        frame,
        GrowingPopup {
            area,
            width_pct: 60,
            title: " Ctrl+R — reply ".to_string(),
            content,
            footer,
            focus_line: 0,
            theme,
        },
    );
}

fn draw_form(frame: &mut Frame, form: &Form, theme: Theme) -> Rect {
    let area = frame.area();

    let mut content: Vec<Line<'static>> = Vec::new();
    content.push(Line::from(Span::styled(form.title, theme.title())));
    content.push(Line::from(""));
    let mut focus_line = 0usize;
    for (i, field) in form.fields.iter().enumerate() {
        if i < form.current {
            content.push(Line::from(Span::styled(
                format!("  ✓ {field}: {}", form.values[i]),
                Style::default().fg(theme.success),
            )));
        } else if i == form.current {
            focus_line = content.len();
            let mut spans = vec![Span::styled(
                format!("> {field}: "),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )];
            push_caret_line(&mut spans, &form.input, Style::default().fg(theme.accent));
            content.push(Line::from(spans));
        } else {
            content.push(Line::from(format!("  {field}")));
        }
    }

    let footer = vec![
        Line::from(""),
        Line::from(Span::styled(
            " Enter: next · Esc: cancel ",
            Style::default().fg(theme.muted),
        )),
    ];

    draw_growing_popup(
        frame,
        GrowingPopup {
            area,
            width_pct: 70,
            title: " Step-by-step form ".to_string(),
            content,
            footer,
            focus_line,
            theme,
        },
    )
}

/// Number of list rows the wizard shows at once.
const WIZARD_VISIBLE: usize = 12;

fn draw_wizard(
    frame: &mut Frame,
    wizard: &Wizard,
    theme: Theme,
    state: &throbber_widgets_tui::ThrobberState,
) -> Rect {
    let area = frame.area();

    let mut content: Vec<Line<'static>> = Vec::new();
    content.push(Line::from(Span::styled(wizard.step.title(), theme.title())));
    content.push(Line::from(""));
    let mut focus_line = 0usize;

    if wizard.step == WizardStep::Dir {
        focus_line = content.len();
        let mut spans = vec![Span::styled(
            "> Directory: ",
            Style::default().fg(theme.accent),
        )];
        push_caret_line(&mut spans, &wizard.dir, Style::default().fg(theme.accent));
        content.push(Line::from(spans));
    } else if wizard.loading {
        content.push(Line::from(vec![
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
            if idx == wizard.selected {
                focus_line = content.len();
            }
            content.push(Line::from(Span::styled(format!(" {label} "), style)));
        }
        if wizard.choices.is_empty() {
            content.push(Line::from(Span::styled(
                "(nothing available)",
                Style::default().fg(theme.muted),
            )));
        }
    }

    if let Some(err) = &wizard.error {
        content.push(Line::from(Span::styled(
            err.clone(),
            Style::default().fg(theme.danger),
        )));
    }

    let footer = vec![
        Line::from(""),
        Line::from(Span::styled(
            " ↑/↓ select · Enter: next · Esc: cancel ",
            Style::default().fg(theme.muted),
        )),
    ];

    draw_growing_popup(
        frame,
        GrowingPopup {
            area,
            width_pct: 70,
            title: " New one-shot task ".to_string(),
            content,
            footer,
            focus_line,
            theme,
        },
    )
}

fn draw_task_vars(frame: &mut Frame, form: &TaskVarsForm, theme: Theme) -> Rect {
    let area = frame.area();

    let mut content: Vec<Line<'static>> = Vec::new();
    content.push(Line::from(Span::styled(
        format!(" Task input · {} ", form.task),
        theme.title(),
    )));
    content.push(Line::from(""));

    let mut focus_line = 0usize;
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
            if current {
                focus_line = content.len();
            }
            content.push(Line::from(Span::styled(label, label_style)));
            let selected = form.choice_selected.get(i).copied().unwrap_or(0);
            for (j, choice) in choices.iter().enumerate() {
                let style = if j == selected {
                    theme.selected()
                } else {
                    Style::default()
                };
                content.push(Line::from(Span::styled(format!("    {choice}"), style)));
            }
            continue;
        }

        let empty = TextBuffer::default();
        let buf = form.values.get(i).unwrap_or(&empty);
        let lines = display_lines(buf);
        for (k, line) in lines.iter().enumerate() {
            if current && line.caret {
                focus_line = content.len();
            }
            let mut spans: Vec<Span> = Vec::new();
            if k == 0 {
                spans.push(Span::styled(label.clone(), label_style));
            } else {
                spans.push(Span::raw("    "));
            }
            if buf.is_empty() && !current && lines.len() == 1 {
                spans.push(Span::styled("(empty)", Style::default().fg(theme.muted)));
            } else {
                spans.push(Span::raw(line.before.clone()));
                if current && line.caret {
                    spans.push(Span::styled("█", Style::default().fg(theme.accent)));
                }
                spans.push(Span::raw(line.after.clone()));
            }
            content.push(Line::from(spans));
        }
    }

    if let Some(err) = &form.error {
        content.push(Line::from(Span::styled(
            err.clone(),
            Style::default().fg(theme.danger),
        )));
    }

    let footer = vec![
        Line::from(""),
        Line::from(Span::styled(
            " Tab/↑↓ move · Enter next · Ctrl+Enter submit · Esc cancel ",
            Style::default().fg(theme.muted),
        )),
    ];

    draw_growing_popup(
        frame,
        GrowingPopup {
            area,
            width_pct: 70,
            title: " Task input ".to_string(),
            content,
            footer,
            focus_line,
            theme,
        },
    )
}

/// Grouped keybinding reference shown by `?`. Each row is `(key, description)`.
const HELP_SECTIONS: &[(&str, &[(&str, &str)])] = &[
    (
        "Global",
        &[
            ("Tab / →", "next tab"),
            ("Shift+Tab / ←", "previous tab"),
            ("Ctrl+P", "open the action menu"),
            ("Ctrl+O", "pick a live/retained agent session"),
            ("Ctrl+R", "answer a pending agent prompt"),
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
            ("(favetto focus) Ctrl+O", "switch to another session"),
            (
                "(pending prompt) Ctrl+R",
                "answer the agent's permission/dialog prompt",
            ),
            ("(favetto focus) Ctrl+Q", "leave the panel"),
            ("(favetto focus) Ctrl+N", "start a new session"),
            ("(favetto focus) Esc", "back to Tasks"),
            ("headless runs", "shown read-only; keys are not forwarded"),
        ],
    ),
    (
        "Text fields (task input / forms / wizard)",
        &[
            ("← / →", "move the caret by character"),
            ("Alt+← / Alt+→ (or Ctrl+←/→)", "move by word"),
            ("Home / End (or Ctrl+A / Ctrl+E)", "start / end of line"),
            (
                "↑ / ↓",
                "move line in multiline values, else previous/next field",
            ),
            ("Backspace / Delete", "delete before / at the caret"),
            ("Alt+Backspace / Ctrl+W", "delete the word before"),
            ("Ctrl+U / Ctrl+K", "delete to line start / end"),
            ("Shift+Enter / Alt+Enter", "newline in multiline values"),
            ("Ctrl+Enter", "submit the form"),
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

/// Arguments for [`draw_workflow`]: the cached structured graph (when present),
/// the raw DOT fallback, and the scroll offsets the overlay mutates in place.
struct WorkflowOverlay<'a> {
    scroll: &'a mut u16,
    hscroll: &'a mut u16,
    lines: Option<&'a [String]>,
    note: Option<&'a str>,
    dot: Option<&'a str>,
    path: Option<&'a str>,
    theme: Theme,
}

/// Draw the floating workflow overlay. When the structured graph was rendered,
/// shows the cached box-drawing rows and scrolls both axes; otherwise falls back
/// to the wrapped raw DOT (with the reason as a leading note line).
fn draw_workflow(frame: &mut Frame, overlay: WorkflowOverlay<'_>) {
    let WorkflowOverlay {
        scroll,
        hscroll,
        lines,
        note,
        dot,
        path,
        theme,
    } = overlay;
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
        Constraint::Length(20),
        Constraint::Length(24),
        Constraint::Length(12),
        Constraint::Length(14),
        Constraint::Length(18),
        Constraint::Length(8),
        Constraint::Min(0),
    ];
    let header = Row::new(vec![
        "ID", "TASK", "SESSION", "STATUS", "ACTIVITY", "USAGE", "AGE", "ERROR",
    ])
    .style(theme.table_header());

    let state = app.throbber_state.clone();
    let rows: Vec<Row> = app
        .tasks
        .iter()
        .map(|t| {
            let session = app.task_session(&t.id.to_string());
            let activity = activity_cell(session.and_then(|s| s.activity.as_ref()));
            let usage = usage_cell(session.and_then(|s| s.usage.as_ref()));
            Row::new(vec![
                Cell::from(short_id(&t.id)),
                Cell::from(t.name.clone()),
                Cell::from(truncate_ellipsis(
                    t.session_title.as_deref().unwrap_or(""),
                    22,
                )),
                Cell::from(status_line(t.status, theme, &state)),
                Cell::from(activity),
                Cell::from(usage),
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
            "{name} · {state}{extra} · Ctrl+O sessions · Ctrl+N new · Ctrl+Q leave "
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
            "The embedded agent terminal appears here. Select a task and press Enter to launch the configured agent, or Ctrl+O to switch between running sessions.",
            Style::default().fg(theme.muted),
        )))
        .wrap(Wrap { trim: true });
        frame.render_widget(hint, inner_area);
        return;
    }

    // A headless run has no interactive view: render its structured state, never
    // the raw machine-output PTY (JSON event stream).
    if app.agent_structured {
        draw_agent_structured(frame, app, inner_area, theme);
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

/// The Agent-panel body for a headless run that has no interactive view: a
/// structured state snapshot instead of the machine PTY's raw JSON stream.
fn draw_agent_structured(frame: &mut Frame, app: &App, area: Rect, theme: Theme) {
    let session = app
        .agent_session_id
        .as_deref()
        .and_then(|id| app.agent_sessions.get(id));
    let name = app.agent_name.as_deref().unwrap_or("agent");
    let state = if app.agent_running {
        "running"
    } else {
        "finished"
    };
    let heading = if app.agent_running {
        "Running headless — no interactive view available"
    } else {
        "Headless run finished — no interactive view available"
    };
    let mut lines = vec![
        Line::from(Span::styled(
            heading,
            Style::default()
                .fg(theme.warning)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!("Agent     {name}")),
        Line::from(format!("State     {state}")),
    ];
    if !app.agent_status.is_empty() {
        lines.push(Line::from(format!("Status    {}", app.agent_status)));
    }
    if let Some(session) = session {
        lines.push(Line::from(format!(
            "Activity  {}",
            activity_cell(session.activity.as_ref())
        )));
        lines.push(Line::from(format!(
            "Usage     {}",
            usage_cell(session.usage.as_ref())
        )));
        if let Some(session_id) = &session.session_id {
            lines.push(Line::from(format!("Session   {session_id}")));
        }
    }
    lines.push(Line::from(""));
    let note = if app.agent_running {
        "The run continues in the background; its output feeds agent state, usage \
         and the task result. This agent cannot open an interactive session while \
         the run is in flight."
    } else {
        "The run has finished; its structured output and result are on the task. \
         This agent has no interactive session to reopen."
    };
    lines.push(Line::from(Span::styled(
        note,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
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

    let mut hint = if app.tab == Tab::Agent {
        if agent_focused {
            "[Ctrl+Y] favetto keys".to_string()
        } else {
            "[Ctrl+Y] agent keys  [Ctrl+O] sessions  [Ctrl+Q] leave  [?] help".to_string()
        }
    } else {
        "[↑↓] select  [Enter] agent  [Tab] switch  [q] quit  [?] help".to_string()
    };
    if app.pending_request().is_some() {
        hint.push_str("  [Ctrl+R] answer");
    }

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

    if let Some(err) = &app.agent_error {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("⚠ agent: {err}"),
            Style::default()
                .fg(theme.danger)
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
#[path = "ui_tests.rs"]
mod tests;
