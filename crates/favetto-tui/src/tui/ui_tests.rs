use super::*;
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use std::collections::BTreeMap;

use favetto_core::model::AgentCapabilities;

fn session(id: &str, agent: &str, headless: bool, running: bool) -> AgentSessionInfo {
    AgentSessionInfo {
        id: id.to_string(),
        agent: agent.to_string(),
        task_id: None,
        running,
        headless,
        session_id: None,
        awaiting_input: None,
    }
}

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
                input: TextBuffer::default(),
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
                dir: TextBuffer::default(),
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
                dir: TextBuffer::default(),
                loading: true,
                error: Some("boom".to_string()),
            }),
            Popup::TaskVars(TaskVarsForm {
                task: "issue".to_string(),
                vars: vec![favetto_core::tasks::TaskVar {
                    name: "description".to_string(),
                    prompt: "Describe".to_string(),
                    default: Some("draft".to_string()),
                    required: true,
                    multiline: false,
                    var_type: favetto_core::tasks::VarType::String,
                    choices: None,
                }],
                current: 0,
                values: vec![TextBuffer::new("draft")],
                choice_selected: vec![0],
                error: None,
            }),
            Popup::Help { scroll: 0 },
            Popup::Workflow {
                scroll: 0,
                hscroll: 0,
            },
            Popup::Sessions {
                selected: 0,
                sessions: vec![
                    session("s1", "opencode", false, true),
                    session("s2", "pi", true, false),
                ],
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
        dir: TextBuffer::default(),
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
fn status_bar_surfaces_agent_errors() {
    let mut app = App::new();
    app.agent_error = Some("no agent sessions to switch to".to_string());
    let text = render_text(&mut app, 140, 24);
    assert!(text.contains("agent:"), "agent error missing: {text:?}");
    assert!(
        text.contains("no agent sessions"),
        "agent error text missing: {text:?}"
    );
}

#[test]
fn session_picker_renders_each_session() {
    let mut app = App::new();
    app.open_sessions(vec![
        session("s1", "opencode", false, true),
        session("s2", "pi", true, false),
    ]);
    let text = render_text(&mut app, 100, 24);
    assert!(text.contains("Ctrl+O"), "picker title missing: {text:?}");
    assert!(text.contains("opencode"), "first agent missing: {text:?}");
    assert!(text.contains("pi"), "second agent missing: {text:?}");
    assert!(
        text.contains("headless"),
        "headless state missing: {text:?}"
    );
}

#[test]
fn agent_tab_marks_a_headless_session_read_only() {
    let mut app = App::new();
    app.tab = Tab::Agent;
    app.open_agent(session("s1", "pi", true, true), b"");
    let text = render_text(&mut app, 100, 24);
    assert!(
        text.contains("read-only"),
        "read-only note missing: {text:?}"
    );
    assert!(text.contains("Ctrl+O"), "sessions hint missing: {text:?}");
}

#[test]
fn help_overlay_lists_session_picker() {
    let mut app = App::new();
    app.popup = Popup::Help { scroll: 0 };
    let text = render_text(&mut app, 100, 40);
    assert!(
        text.contains("Ctrl+O"),
        "session picker help row missing: {text:?}"
    );
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

#[test]
fn floating_preview_captures_wheel_over_drawn_rect() {
    use ratatui::crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};

    let mut app = App::new();
    app.tab = Tab::Catalog;
    app.set_catalog(vec![
        catalog_entry("a"),
        catalog_entry("b"),
        catalog_entry("c"),
        catalog_entry("d"),
    ]);
    let long = (0..80).map(|i| format!("line {i}\n")).collect::<String>();
    app.catalog_preview = Some(("a".to_string(), long));
    app.catalog_selected = 0;

    // Draw at a width below the dock threshold: the preview floats over the
    // tree, so `catalog_preview_area` is inside `catalog_geom.inner`.
    let _ = render_text(&mut app, CATALOG_PREVIEW_DOCK_MIN_WIDTH - 1, 30);
    let preview = app.catalog_preview_area.expect("floating preview rect");
    let before = app.catalog_selected;

    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: preview.x + 1,
        row: preview.y + 1,
        modifiers: KeyModifiers::empty(),
    });

    assert_eq!(app.catalog_selected, before, "tree must not move");
    assert_eq!(app.catalog_preview_scroll, 3);
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
        kind: EventKind::Unknown,
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
        parent_id: None,
        root_id: None,
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
    use favetto_core::tasks::{TaskVar, VarType};

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
        values: vec![TextBuffer::new("placeholder")],
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

#[test]
fn task_vars_popup_keeps_cursor_visible_for_long_multiline_value() {
    use favetto_core::tasks::{TaskVar, VarType};

    let mut value = (0..30)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    value.push_str("\nZZZTAILZZZ");

    let mut app = App::new();
    app.popup = Popup::TaskVars(TaskVarsForm {
        task: "open_github_issue".to_string(),
        vars: vec![TaskVar {
            name: "body".to_string(),
            prompt: "Body".to_string(),
            default: None,
            required: true,
            multiline: true,
            var_type: VarType::String,
            choices: None,
        }],
        current: 0,
        values: vec![TextBuffer::new(value)],
        choice_selected: vec![0],
        error: None,
    });

    let text = render_text(&mut app, 80, 24);
    assert!(text.contains('█'), "cursor missing: {text:?}");
    assert!(text.contains("ZZZTAILZZZ"), "tail clipped: {text:?}");
    assert!(text.contains("Ctrl+Enter"), "footer not docked: {text:?}");
}

#[test]
fn task_vars_short_value_keeps_fixed_height() {
    use favetto_core::tasks::{TaskVar, VarType};

    let mut app = App::new();
    app.popup = Popup::TaskVars(TaskVarsForm {
        task: "open_github_issue".to_string(),
        vars: vec![TaskVar {
            name: "title".to_string(),
            prompt: "Title".to_string(),
            default: Some("hi".to_string()),
            required: true,
            multiline: false,
            var_type: VarType::String,
            choices: None,
        }],
        current: 0,
        values: vec![TextBuffer::new("hi")],
        choice_selected: vec![0],
        error: None,
    });

    let theme = app.theme;
    let backend = TestBackend::new(100, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut rect = Rect::new(0, 0, 0, 0);
    if let Popup::TaskVars(form) = &app.popup {
        terminal
            .draw(|f| rect = draw_task_vars(f, form, theme))
            .unwrap();
    } else {
        panic!("expected task vars popup");
    }
    assert_eq!(rect.height, 7, "short var height changed: {rect:?}");

    let text = render_text(&mut app, 100, 40);
    assert!(text.contains('█'), "cursor missing: {text:?}");
    assert!(text.contains("hi"), "value missing: {text:?}");
}

#[test]
fn task_vars_renders_caret_in_the_middle_of_a_value() {
    use favetto_core::tasks::{TaskVar, VarType};

    let mut buffer = TextBuffer::new("abcd");
    buffer.move_left(); // caret before the trailing 'd'
    let mut app = App::new();
    app.popup = Popup::TaskVars(TaskVarsForm {
        task: "issue".to_string(),
        vars: vec![TaskVar {
            name: "title".to_string(),
            prompt: "Title".to_string(),
            default: None,
            required: true,
            multiline: false,
            var_type: VarType::String,
            choices: None,
        }],
        current: 0,
        values: vec![buffer],
        choice_selected: vec![0],
        error: None,
    });
    let text = render_text(&mut app, 100, 40);
    assert!(text.contains("abc█d"), "caret not mid-line: {text:?}");
}

#[test]
fn form_renders_caret_in_the_middle_of_the_input() {
    use super::super::app::FormKind;

    let mut input = TextBuffer::new("abcd");
    input.move_left(); // caret before the trailing 'd'
    let mut app = App::new();
    app.popup = Popup::Form(Form {
        kind: FormKind::AddTask,
        title: "Add task to catalog",
        fields: vec!["Task name", "Prompt"],
        current: 1,
        values: vec!["my-task".to_string()],
        input,
    });
    let text = render_text(&mut app, 80, 24);
    assert!(text.contains("abc█d"), "caret not mid-line: {text:?}");
}

#[test]
fn form_popup_long_prompt_keeps_cursor_visible() {
    use super::super::app::FormKind;

    let fields = vec!["Task name", "Prompt"];
    let mut input = String::new();
    for i in 0..40 {
        input.push_str(&format!("word{i} "));
    }
    input.push_str("ZZZTAILZZZ");

    let form = Form {
        kind: FormKind::AddTask,
        title: "Add task to catalog",
        fields,
        current: 1,
        values: vec!["my-task".to_string()],
        input: TextBuffer::new(input),
    };

    let mut app = App::new();
    app.popup = Popup::Form(form);
    let text = render_text(&mut app, 80, 24);
    assert!(text.contains('█'), "cursor missing: {text:?}");
    assert!(text.contains("ZZZTAILZZZ"), "tail clipped: {text:?}");
    assert!(text.contains("Enter: next"), "footer not docked: {text:?}");

    let theme = app.theme;
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut rect = Rect::new(0, 0, 0, 0);
    if let Popup::Form(form) = &app.popup {
        terminal.draw(|f| rect = draw_form(f, form, theme)).unwrap();
    } else {
        panic!("expected form popup");
    }
    assert!(rect.y + rect.height <= 24, "form overflows frame: {rect:?}");
    assert!(rect.x + rect.width <= 80, "form overflows frame: {rect:?}");
}

#[test]
fn scrolling_body_layout_clamps_scroll() {
    let (rect, scroll) = scrolling_body_layout(Rect::new(0, 0, 80, 24), 70, 40, 2, 39);
    assert!(rect.height <= 20, "popup exceeds frame: {rect:?}");
    let viewport = rect.height.saturating_sub(2).saturating_sub(2);
    assert!(viewport > 0);
    assert!(scroll + viewport > 39);
    assert!(scroll <= 40 - viewport);
}
