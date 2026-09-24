use super::*;
use chrono::Utc;
use favetto_core::model::{
    AgentActivity, AgentUsage, AwaitingInputKind, EventKind, InputReply, InputRequest, TaskStatus,
};

use crate::tasks::VarType;

fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, mods)
}

fn click(col: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: col,
        row,
        modifiers: KeyModifiers::empty(),
    }
}

fn geom(inner: Rect, offset: usize, len: usize) -> ListGeometry {
    ListGeometry { inner, offset, len }
}

fn task(name: &str) -> Task {
    Task {
        id: uuid::Uuid::new_v4(),
        name: name.to_string(),
        status: TaskStatus::Pending,
        attempt: 0,
        input: serde_json::json!({}),
        output: None,
        dedupe_key: None,
        created_at: Utc::now(),
        started_at: None,
        finished_at: None,
        error: None,
        failure: None,
        session_id: None,
        session_title: None,
        parent_id: None,
        root_id: None,
        interactive: false,
    }
}

fn catalog_entry(name: &str) -> CatalogEntry {
    CatalogEntry {
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

fn agent_session(id: &str, headless: bool, running: bool, awaiting: bool) -> AgentSessionInfo {
    AgentSessionInfo {
        id: id.to_string(),
        agent: "demo".to_string(),
        task_id: None,
        running,
        headless,
        session_id: None,
        awaiting_input: awaiting.then(|| favetto_core::model::AwaitingInputReason {
            kind: favetto_core::model::AwaitingInputKind::Other,
            message: "allow?".to_string(),
            request_id: None,
            options: Vec::new(),
            allow_always: false,
        }),
        activity: None,
        usage: None,
    }
}

#[test]
fn open_agent_marks_headless_runs_read_only() {
    let mut app = App::new();
    app.open_agent(agent_session("s1", true, true, false), b"");
    assert!(app.agent_read_only);
    assert!(!app.agent_capture);
    assert_eq!(app.agent_session_id.as_deref(), Some("s1"));
    assert!(app.agent_status.contains("read-only"));

    // Ctrl+Y cannot hand a read-only PTY the keyboard, and keys go nowhere.
    app.handle_key(key(KeyCode::Char('y'), KeyModifiers::CONTROL));
    assert!(!app.agent_capture);
    assert!(matches!(
        app.handle_key(key(KeyCode::Char('x'), KeyModifiers::empty())),
        UiAction::None
    ));
}

#[test]
fn open_agent_keeps_a_blocked_headless_run_writable() {
    let mut app = App::new();
    app.open_agent(agent_session("s2", true, true, true), b"");
    assert!(!app.agent_read_only);
    assert!(app.agent_capture);
    match app.handle_key(key(KeyCode::Char('y'), KeyModifiers::empty())) {
        UiAction::AgentInput(bytes) => assert_eq!(bytes, b"y".to_vec()),
        _ => panic!("expected AgentInput"),
    }
}

#[test]
fn open_agent_keeps_interactive_sessions_writable() {
    let mut app = App::new();
    app.open_agent(agent_session("s3", false, true, false), b"");
    assert!(!app.agent_read_only);
    assert!(app.agent_capture);
    assert!(app.agent_status.is_empty());
}

#[test]
fn open_agent_clears_a_previous_error() {
    let mut app = App::new();
    app.agent_error = Some("boom".to_string());
    app.open_agent(agent_session("s4", false, true, false), b"");
    assert!(app.agent_error.is_none());
}

#[test]
fn ctrl_o_opens_session_picker_action() {
    let mut app = App::new();
    assert!(matches!(
        app.handle_key(key(KeyCode::Char('o'), KeyModifiers::CONTROL)),
        UiAction::OpenSessions
    ));
}

#[test]
fn ctrl_o_is_forwarded_to_captured_agent() {
    let mut app = App::new();
    app.tab = Tab::Agent;
    app.agent_capture = true;
    match app.handle_key(key(KeyCode::Char('o'), KeyModifiers::CONTROL)) {
        UiAction::AgentInput(bytes) => assert_eq!(bytes, vec![0x0f]),
        _ => panic!("expected AgentInput"),
    }
    assert!(matches!(app.popup, Popup::None));
}

#[test]
fn session_picker_attaches_the_selected_session() {
    let mut app = App::new();
    app.open_sessions(vec![
        agent_session("a", false, true, false),
        agent_session("b", true, true, false),
    ]);
    assert!(matches!(app.popup, Popup::Sessions { selected: 0, .. }));

    app.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
    match app.handle_key(key(KeyCode::Enter, KeyModifiers::empty())) {
        UiAction::AttachSession(sid) => assert_eq!(sid, "b"),
        _ => panic!("expected AttachSession"),
    }
    assert!(matches!(app.popup, Popup::None));
}

#[test]
fn session_picker_closes_and_reports_when_empty() {
    let mut app = App::new();
    app.open_sessions(Vec::new());
    assert!(matches!(app.popup, Popup::None));
    assert!(app.agent_error.is_some());
}

/// A permission request shaped like the adapters produce (opencode/claude).
fn permission_request(options: Vec<&str>, allow_always: bool) -> InputRequest {
    InputRequest {
        id: "perm_1".to_string(),
        kind: AwaitingInputKind::Permission,
        message: "Allow bash?".to_string(),
        options: options.into_iter().map(str::to_string).collect(),
        allow_always,
    }
}

fn waiting_session(id: &str, request: InputRequest) -> AgentSessionInfo {
    let mut session = agent_session(id, false, true, false);
    session.activity = Some(AgentActivity::Waiting { request });
    session
}

#[test]
fn agent_state_push_folds_activity_and_usage() {
    let mut app = App::new();
    app.set_agent_sessions(vec![agent_session("s1", false, true, false)]);
    app.handle_notification(Notification {
        method: push::AGENT_STATE.to_string(),
        params: serde_json::json!({
            "session_id": "s1",
            "activity": { "kind": "tool", "name": "bash" },
            "usage": { "input_tokens": 1200, "output_tokens": 34, "cost_usd": 0.25 },
        }),
    });
    let session = app.agent_sessions.get("s1").unwrap();
    assert_eq!(
        session.activity,
        Some(AgentActivity::Tool {
            name: "bash".to_string(),
            description: None,
        })
    );
    let usage = session.usage.as_ref().expect("usage folded");
    assert_eq!(usage.input_tokens, 1200);
    assert_eq!(usage.cost_usd, Some(0.25));

    // A frame for an unknown session is ignored, not invented.
    app.handle_notification(Notification {
        method: push::AGENT_STATE.to_string(),
        params: serde_json::json!({ "session_id": "nope", "activity": { "kind": "idle" } }),
    });
    assert_eq!(app.agent_sessions.len(), 1);
}

#[test]
fn task_session_joins_by_task_id_preferring_running() {
    let mut app = App::new();
    let mut stopped = agent_session("a", false, false, false);
    stopped.task_id = Some("t1".to_string());
    let mut running = agent_session("b", false, true, false);
    running.task_id = Some("t1".to_string());
    let mut other = agent_session("c", false, true, false);
    other.task_id = Some("t2".to_string());
    app.set_agent_sessions(vec![stopped, running, other]);

    assert_eq!(app.task_session("t1").map(|s| s.id.as_str()), Some("b"));
    assert_eq!(app.task_session("t2").map(|s| s.id.as_str()), Some("c"));
    assert!(app.task_session("missing").is_none());
}

#[test]
fn ctrl_r_opens_reply_only_with_a_structured_request() {
    let mut app = App::new();
    // No attached session: Ctrl+R is inert.
    app.handle_key(key(KeyCode::Char('r'), KeyModifiers::CONTROL));
    assert!(matches!(app.popup, Popup::None));

    // Screen-detected awaiting input (no request_id) is not remotely answerable.
    let mut screen = agent_session("s1", false, true, true);
    screen.awaiting_input.as_mut().unwrap().request_id = None;
    app.set_agent_sessions(vec![screen]);
    app.agent_session_id = Some("s1".to_string());
    app.handle_key(key(KeyCode::Char('r'), KeyModifiers::CONTROL));
    assert!(matches!(app.popup, Popup::None));

    // A Waiting activity opens the precise reply prompt.
    app.set_agent_sessions(vec![waiting_session(
        "s2",
        permission_request(vec!["Allow once", "Reject"], true),
    )]);
    app.agent_session_id = Some("s2".to_string());
    app.handle_key(key(KeyCode::Char('r'), KeyModifiers::CONTROL));
    assert!(matches!(app.popup, Popup::Reply(_)));
}

#[test]
fn ctrl_r_uses_the_structured_awaiting_input_fallback() {
    let mut app = App::new();
    let mut session = agent_session("s1", false, true, true);
    session.activity = None;
    session.awaiting_input.as_mut().unwrap().request_id = Some("perm_9".to_string());
    session.awaiting_input.as_mut().unwrap().options = vec!["Allow once".to_string()];
    app.set_agent_sessions(vec![session]);
    app.agent_session_id = Some("s1".to_string());

    match app.pending_request() {
        Some((sid, request)) => {
            assert_eq!(sid, "s1");
            assert_eq!(request.id, "perm_9");
            assert_eq!(request.options, vec!["Allow once".to_string()]);
        }
        None => panic!("expected a structured fallback request"),
    }
}

#[test]
fn reply_popup_maps_permission_selection() {
    let mut app = App::new();
    app.set_agent_sessions(vec![waiting_session(
        "s1",
        permission_request(vec!["Allow once", "Allow always", "Reject"], true),
    )]);
    app.agent_session_id = Some("s1".to_string());

    app.handle_key(key(KeyCode::Char('r'), KeyModifiers::CONTROL));
    app.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
    match app.handle_key(key(KeyCode::Enter, KeyModifiers::empty())) {
        UiAction::Reply {
            session_id,
            request_id,
            reply,
        } => {
            assert_eq!(session_id, "s1");
            assert_eq!(request_id, "perm_1");
            assert_eq!(reply, InputReply::Always);
        }
        _ => panic!("expected Reply"),
    }
    assert!(matches!(app.popup, Popup::None));
}

#[test]
fn reply_for_is_kind_aware() {
    let permission = permission_request(vec!["Allow once", "Allow always", "Reject"], true);
    assert_eq!(reply_for(&permission, 0, ""), InputReply::Once);
    assert_eq!(reply_for(&permission, 1, ""), InputReply::Always);
    assert_eq!(reply_for(&permission, 2, ""), InputReply::Reject);

    let confirmation = InputRequest {
        id: "c".to_string(),
        kind: AwaitingInputKind::Confirmation,
        message: "Proceed?".to_string(),
        options: vec!["Yes".to_string(), "No".to_string()],
        allow_always: false,
    };
    assert_eq!(
        reply_for(&confirmation, 0, ""),
        InputReply::Confirmed { confirmed: true }
    );
    assert_eq!(
        reply_for(&confirmation, 1, ""),
        InputReply::Confirmed { confirmed: false }
    );

    let choice = InputRequest {
        id: "ch".to_string(),
        kind: AwaitingInputKind::Choice,
        message: "Pick".to_string(),
        options: vec!["A".to_string(), "B".to_string()],
        allow_always: false,
    };
    assert_eq!(
        reply_for(&choice, 1, ""),
        InputReply::Value {
            value: "B".to_string()
        }
    );

    let free = InputRequest {
        options: Vec::new(),
        ..choice
    };
    assert_eq!(
        reply_for(&free, 0, "typed"),
        InputReply::Value {
            value: "typed".to_string()
        }
    );
}

#[test]
fn reply_popup_free_form_types_a_value() {
    let mut app = App::new();
    let request = InputRequest {
        id: "q1".to_string(),
        kind: AwaitingInputKind::Other,
        message: "Your name?".to_string(),
        options: Vec::new(),
        allow_always: false,
    };
    app.set_agent_sessions(vec![waiting_session("s1", request)]);
    app.agent_session_id = Some("s1".to_string());

    app.handle_key(key(KeyCode::Char('r'), KeyModifiers::CONTROL));
    for c in "abc".chars() {
        app.handle_key(key(KeyCode::Char(c), KeyModifiers::empty()));
    }
    match app.handle_key(key(KeyCode::Enter, KeyModifiers::empty())) {
        UiAction::Reply { reply, .. } => {
            assert_eq!(
                reply,
                InputReply::Value {
                    value: "abc".to_string()
                }
            )
        }
        _ => panic!("expected Reply"),
    }
}

#[test]
fn activity_and_usage_cells_render_compactly() {
    assert_eq!(activity_cell(None), "—");
    assert_eq!(activity_cell(Some(&AgentActivity::Thinking)), "thinking");
    assert_eq!(
        activity_cell(Some(&AgentActivity::Tool {
            name: "read".to_string(),
            description: None,
        })),
        "tool: read"
    );
    assert_eq!(usage_cell(None), "—");
    assert_eq!(usage_cell(Some(&AgentUsage::default())), "—");
    let usage = AgentUsage {
        input_tokens: 1500,
        output_tokens: 20,
        reasoning_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        cost_usd: Some(0.5),
    };
    assert_eq!(usage_cell(Some(&usage)), "1.5k/20 tok $0.5000");
}

fn event(id: i64, kind: EventKind, payload: serde_json::Value) -> Event {
    Event {
        id,
        kind,
        payload,
        created_at: Utc::now(),
    }
}

#[test]
fn task_finished_enqueues_success_or_failure_cue() {
    let mut app = App::new();
    app.ingest_event(event(
        1,
        EventKind::TaskFinished,
        serde_json::json!({ "success": true }),
    ));
    assert_eq!(app.take_sound_cues(), vec![SoundCue::TaskFinished]);

    app.ingest_event(event(
        2,
        EventKind::TaskFinished,
        serde_json::json!({ "success": false }),
    ));
    assert_eq!(app.take_sound_cues(), vec![SoundCue::TaskFailed]);
}

#[test]
fn only_task_finished_maps_to_finish_cues() {
    let mut app = App::new();
    app.ingest_event(event(1, EventKind::TaskCompleted, serde_json::json!({})));
    app.ingest_event(event(2, EventKind::TaskFailed, serde_json::json!({})));
    // `task_started` is mapped; whether it sounds is a player-level concern.
    app.ingest_event(event(3, EventKind::TaskStarted, serde_json::json!({})));
    assert_eq!(app.take_sound_cues(), vec![SoundCue::TaskStarted]);
}

#[test]
fn cue_for_event_maps_awaiting_input() {
    assert_eq!(
        cue_for_event(&event(
            1,
            EventKind::TaskAwaitingInput,
            serde_json::json!({})
        )),
        Some(SoundCue::AwaitingInput)
    );
    // And ingesting it queues the cue on the app.
    let mut app = App::new();
    app.ingest_event(event(
        1,
        EventKind::TaskAwaitingInput,
        serde_json::json!({}),
    ));
    assert_eq!(app.take_sound_cues(), vec![SoundCue::AwaitingInput]);
}

#[test]
fn agent_exit_push_enqueues_attention() {
    let mut app = App::new();
    app.handle_notification(Notification {
        method: push::AGENT_EXIT.to_string(),
        params: serde_json::json!({ "session_id": "s1", "code": 0 }),
    });
    assert_eq!(app.take_sound_cues(), vec![SoundCue::Attention]);
}

#[test]
fn suppressed_replay_does_not_enqueue_cues() {
    let mut app = App::new();
    app.sound_suppressed = true;
    app.ingest_event(event(
        1,
        EventKind::TaskFinished,
        serde_json::json!({ "success": true }),
    ));
    assert!(app.take_sound_cues().is_empty());
    // The event is still recorded, just not sounded.
    assert_eq!(app.events.len(), 1);
}

#[test]
fn m_key_toggles_mute_unless_agent_captures() {
    let mut app = App::new();
    app.handle_key(key(KeyCode::Char('m'), KeyModifiers::empty()));
    assert!(app.sound_muted);
    app.handle_key(key(KeyCode::Char('M'), KeyModifiers::empty()));
    assert!(!app.sound_muted);

    // Under agent capture the key goes to the PTY instead.
    app.tab = Tab::Agent;
    app.agent_capture = true;
    match app.handle_key(key(KeyCode::Char('m'), KeyModifiers::empty())) {
        UiAction::AgentInput(bytes) => assert_eq!(bytes, b"m".to_vec()),
        _ => panic!("expected AgentInput"),
    }
    assert!(!app.sound_muted);
}

#[test]
fn agent_capture_forwards_keys_favetto_would_otherwise_use() {
    let mut app = App::new();
    app.tab = Tab::Agent;
    app.agent_capture = true;
    // Ctrl+P is normally the favetto menu, but the agent owns the keyboard.
    match app.handle_key(key(KeyCode::Char('p'), KeyModifiers::CONTROL)) {
        UiAction::AgentInput(bytes) => assert_eq!(bytes, vec![0x10]),
        _ => panic!("expected AgentInput"),
    }
}

#[test]
fn focus_toggle_flips_and_is_never_forwarded() {
    let mut app = App::new();
    app.tab = Tab::Agent;
    app.agent_capture = true;
    assert!(matches!(
        app.handle_key(key(KeyCode::Char('y'), KeyModifiers::CONTROL)),
        UiAction::None
    ));
    assert!(!app.agent_capture);
    assert!(matches!(
        app.handle_key(key(KeyCode::Char('y'), KeyModifiers::CONTROL)),
        UiAction::None
    ));
    assert!(app.agent_capture);
}

#[test]
fn favetto_focus_handles_agent_tab_shortcuts() {
    let mut app = App::new();
    app.tab = Tab::Agent;
    app.agent_capture = false;
    assert!(matches!(
        app.handle_key(key(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        UiAction::None
    ));
    assert_eq!(app.tab, Tab::Tasks);
}

#[test]
fn question_mark_opens_and_closes_help() {
    let mut app = App::new();
    assert!(matches!(
        app.handle_key(key(KeyCode::Char('?'), KeyModifiers::empty())),
        UiAction::None
    ));
    assert!(matches!(app.popup, Popup::Help { scroll: 0 }));

    // `?` closes it again.
    app.handle_key(key(KeyCode::Char('?'), KeyModifiers::empty()));
    assert!(matches!(app.popup, Popup::None));

    // `Esc` also closes it.
    app.handle_key(key(KeyCode::Char('?'), KeyModifiers::empty()));
    app.handle_key(key(KeyCode::Esc, KeyModifiers::empty()));
    assert!(matches!(app.popup, Popup::None));

    // Arrows and page keys scroll the content.
    app.handle_key(key(KeyCode::Char('?'), KeyModifiers::empty()));
    app.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
    assert!(matches!(app.popup, Popup::Help { scroll: 1 }));
    app.handle_key(key(KeyCode::PageDown, KeyModifiers::empty()));
    assert!(matches!(app.popup, Popup::Help { scroll: 11 }));
    app.handle_key(key(KeyCode::Up, KeyModifiers::empty()));
    assert!(matches!(app.popup, Popup::Help { scroll: 10 }));
    app.handle_key(key(KeyCode::PageUp, KeyModifiers::empty()));
    assert!(matches!(app.popup, Popup::Help { scroll: 0 }));
    // Scrolling up from the top saturates at zero.
    app.handle_key(key(KeyCode::Up, KeyModifiers::empty()));
    assert!(matches!(app.popup, Popup::Help { scroll: 0 }));
}

#[test]
fn question_mark_is_forwarded_to_captured_agent() {
    let mut app = App::new();
    app.tab = Tab::Agent;
    app.agent_capture = true;
    match app.handle_key(key(KeyCode::Char('?'), KeyModifiers::empty())) {
        UiAction::AgentInput(bytes) => assert_eq!(bytes, b"?".to_vec()),
        _ => panic!("expected AgentInput"),
    }
    assert!(matches!(app.popup, Popup::None));
}

#[test]
fn question_mark_opens_help_on_agent_tab_with_favetto_focus() {
    let mut app = App::new();
    app.tab = Tab::Agent;
    app.agent_capture = false;
    app.handle_key(key(KeyCode::Char('?'), KeyModifiers::empty()));
    assert!(matches!(app.popup, Popup::Help { scroll: 0 }));
}

#[test]
fn question_mark_is_literal_inside_form() {
    let mut app = App::new();
    app.handle_key(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
    app.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
    app.handle_key(key(KeyCode::Enter, KeyModifiers::empty()));
    let Popup::Form(form) = &app.popup else {
        panic!("expected form");
    };
    assert!(matches!(form.kind, FormKind::AddTask));

    app.handle_key(key(KeyCode::Char('?'), KeyModifiers::empty()));
    let Popup::Form(form) = &app.popup else {
        panic!("expected form");
    };
    assert_eq!(form.input, "?");
}

#[test]
fn question_mark_does_not_open_over_menu() {
    let mut app = App::new();
    app.handle_key(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
    assert!(matches!(app.popup, Popup::Menu { .. }));
    app.handle_key(key(KeyCode::Char('?'), KeyModifiers::empty()));
    assert!(matches!(app.popup, Popup::Menu { .. }));
}

#[test]
fn workflow_key_opens_and_closes() {
    let mut app = App::new();
    assert!(matches!(
        app.handle_key(key(KeyCode::Char('w'), KeyModifiers::empty())),
        UiAction::OpenWorkflow
    ));
    assert!(matches!(app.popup, Popup::Workflow { scroll: 0, .. }));

    // `w` closes it again.
    app.handle_key(key(KeyCode::Char('w'), KeyModifiers::empty()));
    assert!(matches!(app.popup, Popup::None));

    // `Esc` also closes it.
    app.handle_key(key(KeyCode::Char('w'), KeyModifiers::empty()));
    app.handle_key(key(KeyCode::Esc, KeyModifiers::empty()));
    assert!(matches!(app.popup, Popup::None));
}

#[test]
fn workflow_key_is_forwarded_to_captured_agent() {
    let mut app = App::new();
    app.tab = Tab::Agent;
    app.agent_capture = true;
    match app.handle_key(key(KeyCode::Char('w'), KeyModifiers::empty())) {
        UiAction::AgentInput(bytes) => assert_eq!(bytes, b"w".to_vec()),
        _ => panic!("expected AgentInput"),
    }
    assert!(matches!(app.popup, Popup::None));
}

#[test]
fn workflow_key_is_literal_inside_another_popup() {
    let mut app = App::new();
    app.popup = Popup::Help { scroll: 0 };
    app.handle_key(key(KeyCode::Char('w'), KeyModifiers::empty()));
    assert!(matches!(app.popup, Popup::Help { scroll: 0 }));
}

#[test]
fn set_workflow_stores_dot_and_path() {
    let mut app = App::new();
    assert!(app.workflow_dot.is_none());
    assert!(app.workflow_path.is_none());
    assert!(app.workflow_lines.is_none());
    let graph = crate::workflow::WorkflowGraph {
        nodes: vec![crate::workflow::WorkflowNode {
            name: "a".to_string(),
            scheduled: false,
            external: false,
        }],
        edges: Vec::new(),
    };
    app.set_workflow(
        "digraph workflow {}".to_string(),
        Some("/tmp/workflow.dot".to_string()),
        Some(graph),
    );
    assert_eq!(app.workflow_dot.as_deref(), Some("digraph workflow {}"));
    assert_eq!(app.workflow_path.as_deref(), Some("/tmp/workflow.dot"));
    assert!(app.workflow_lines.is_some());
    assert!(app.workflow_note.is_none());
}

#[test]
fn set_workflow_without_graph_falls_back_to_dot() {
    let mut app = App::new();
    app.set_workflow("digraph workflow {}".to_string(), None, None);
    assert_eq!(app.workflow_dot.as_deref(), Some("digraph workflow {}"));
    assert!(app.workflow_lines.is_none());
    assert!(
        app.workflow_note.as_deref().unwrap_or("").contains("DOT"),
        "{:?}",
        app.workflow_note
    );
}

#[test]
fn workflow_scroll_via_arrow_keys() {
    let mut app = App::new();
    app.popup = Popup::Workflow {
        scroll: 0,
        hscroll: 0,
    };
    app.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
    assert!(matches!(app.popup, Popup::Workflow { scroll: 1, .. }));
    app.handle_key(key(KeyCode::PageDown, KeyModifiers::empty()));
    assert!(matches!(app.popup, Popup::Workflow { scroll: 11, .. }));
    app.handle_key(key(KeyCode::Up, KeyModifiers::empty()));
    assert!(matches!(app.popup, Popup::Workflow { scroll: 10, .. }));
    app.handle_key(key(KeyCode::PageUp, KeyModifiers::empty()));
    assert!(matches!(app.popup, Popup::Workflow { scroll: 0, .. }));
    // Scrolling up from the top saturates at zero.
    app.handle_key(key(KeyCode::Up, KeyModifiers::empty()));
    assert!(matches!(app.popup, Popup::Workflow { scroll: 0, .. }));

    // Horizontal scrolling uses the same saturation rules.
    app.handle_key(key(KeyCode::Left, KeyModifiers::empty()));
    assert!(matches!(app.popup, Popup::Workflow { hscroll: 0, .. }));
    app.handle_key(key(KeyCode::Right, KeyModifiers::empty()));
    app.handle_key(key(KeyCode::Right, KeyModifiers::empty()));
    assert!(matches!(app.popup, Popup::Workflow { hscroll: 2, .. }));
}

#[test]
fn catalog_preview_scrolls_with_keys_and_wheel() {
    let mut app = App::new();
    app.tab = Tab::Catalog;
    app.catalog = vec![
        CatalogEntry {
            name: "a".to_string(),
            agent: None,
            provider: None,
            model: None,
            cwd: None,
            needs: None,
            vars: Vec::new(),
            prompt: String::new(),
        },
        CatalogEntry {
            name: "b".to_string(),
            agent: None,
            provider: None,
            model: None,
            cwd: None,
            needs: None,
            vars: Vec::new(),
            prompt: String::new(),
        },
    ];
    app.catalog_preview_max_scroll = 100;
    app.catalog_preview_area = Some(Rect {
        x: 40,
        y: 1,
        width: 40,
        height: 20,
    });

    // PageDown/PageUp move by the pane's inner height.
    app.handle_key(key(KeyCode::PageDown, KeyModifiers::empty()));
    assert_eq!(app.catalog_preview_scroll, 18);
    app.handle_key(key(KeyCode::PageUp, KeyModifiers::empty()));
    assert_eq!(app.catalog_preview_scroll, 0);

    // The wheel scrolls three lines at a time when over the pane.
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 50,
        row: 5,
        modifiers: KeyModifiers::empty(),
    });
    assert_eq!(app.catalog_preview_scroll, 3);

    // The scroll is clamped to the content.
    app.handle_key(key(KeyCode::PageDown, KeyModifiers::empty()));
    for _ in 0..20 {
        app.handle_key(key(KeyCode::PageDown, KeyModifiers::empty()));
    }
    assert_eq!(app.catalog_preview_scroll, 100);

    // Changing the selected task resets the scroll to the top.
    app.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
    assert_eq!(app.catalog_preview_scroll, 0);
}

#[test]
fn catalog_preview_toggle_key_flips_and_clears_area() {
    let mut app = App::new();
    app.tab = Tab::Catalog;
    app.catalog_preview_area = Some(Rect {
        x: 0,
        y: 0,
        width: 10,
        height: 10,
    });
    assert!(app.catalog_preview_visible);

    app.handle_key(key(KeyCode::Char('p'), KeyModifiers::empty()));
    assert!(!app.catalog_preview_visible);
    assert!(app.catalog_preview_area.is_none());

    app.handle_key(key(KeyCode::Char('p'), KeyModifiers::empty()));
    assert!(app.catalog_preview_visible);

    // Uppercase works too.
    app.handle_key(key(KeyCode::Char('P'), KeyModifiers::empty()));
    assert!(!app.catalog_preview_visible);
}

#[test]
fn catalog_preview_toggle_only_applies_to_catalog_tab() {
    let mut app = App::new();
    app.tab = Tab::Tasks;
    app.handle_key(key(KeyCode::Char('p'), KeyModifiers::empty()));
    assert!(app.catalog_preview_visible);

    // With the agent capturing keys, `p` is forwarded instead.
    app.tab = Tab::Agent;
    app.agent_capture = true;
    match app.handle_key(key(KeyCode::Char('p'), KeyModifiers::empty())) {
        UiAction::AgentInput(bytes) => assert_eq!(bytes, b"p".to_vec()),
        _ => panic!("expected AgentInput"),
    }
    assert!(app.catalog_preview_visible);
}

#[test]
fn catalog_preview_target_is_none_while_hidden() {
    let mut app = App::new();
    app.tab = Tab::Catalog;
    app.set_catalog(vec![catalog_entry("demo")]);

    assert_eq!(app.catalog_preview_target().as_deref(), Some("demo"));
    app.catalog_preview_visible = false;
    assert_eq!(app.catalog_preview_target(), None);
}

#[test]
fn catalog_preview_is_literal_inside_form() {
    let mut app = App::new();
    app.handle_key(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
    app.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
    app.handle_key(key(KeyCode::Enter, KeyModifiers::empty()));
    let Popup::Form(form) = &app.popup else {
        panic!("expected form");
    };
    assert!(matches!(form.kind, FormKind::AddTask));

    app.handle_key(key(KeyCode::Char('p'), KeyModifiers::empty()));
    let Popup::Form(form) = &app.popup else {
        panic!("expected form");
    };
    assert_eq!(form.input, "p");
    assert!(app.catalog_preview_visible);
}

#[test]
fn catalog_edit_key_maps_to_edit_action() {
    let mut app = App::new();
    app.tab = Tab::Catalog;
    app.set_catalog(vec![catalog_entry("demo")]);

    match app.handle_key(key(KeyCode::Char('e'), KeyModifiers::empty())) {
        UiAction::EditCatalog(name) => assert_eq!(name, "demo"),
        _ => panic!("expected EditCatalog"),
    }

    // The binding is Catalog-only: `e` does nothing on the Tasks tab.
    app.tab = Tab::Tasks;
    assert!(matches!(
        app.handle_key(key(KeyCode::Char('e'), KeyModifiers::empty())),
        UiAction::None
    ));
}

#[test]
fn catalog_edit_key_is_noop_on_folder() {
    let mut app = App::new();
    app.tab = Tab::Catalog;
    app.set_catalog(nested_catalog());
    // Row 1 is the `pipelines` folder header.
    app.catalog_selected = 1;
    assert!(matches!(
        app.catalog_rows().get(1),
        Some(CatalogRow::Folder { .. })
    ));

    assert!(matches!(
        app.handle_key(key(KeyCode::Char('e'), KeyModifiers::empty())),
        UiAction::None
    ));
}

#[test]
fn catalog_edit_key_literal_inside_form() {
    let mut app = App::new();
    app.handle_key(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
    app.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
    app.handle_key(key(KeyCode::Enter, KeyModifiers::empty()));
    let Popup::Form(form) = &app.popup else {
        panic!("expected form");
    };
    assert!(matches!(form.kind, FormKind::AddTask));

    app.handle_key(key(KeyCode::Char('e'), KeyModifiers::empty()));
    let Popup::Form(form) = &app.popup else {
        panic!("expected form");
    };
    assert_eq!(form.input, "e");
}

#[test]
fn catalog_updated_push_marks_catalog_dirty() {
    let mut app = App::new();
    assert!(!app.catalog_dirty);
    app.handle_notification(Notification {
        method: push::CATALOG_UPDATED.to_string(),
        params: serde_json::json!({}),
    });
    assert!(app.catalog_dirty);
}

#[test]
fn set_catalog_clamps_selection_and_refreshes_preview() {
    let mut app = App::new();
    app.catalog_selected = 5;
    app.catalog_preview = Some(("gone".to_string(), "md".to_string()));

    app.set_catalog(vec![CatalogEntry {
        name: "a".to_string(),
        agent: None,
        provider: None,
        model: None,
        cwd: None,
        needs: None,
        vars: Vec::new(),
        prompt: String::new(),
    }]);

    assert_eq!(app.catalog_selected, 0);
    assert!(app.catalog_preview.is_none());

    app.set_catalog(Vec::new());
    assert_eq!(app.catalog_selected, 0);
}

/// A catalog with a root task, nested tasks, and a nested folder.
fn nested_catalog() -> Vec<CatalogEntry> {
    vec![
        catalog_entry("b"),
        catalog_entry("pipelines/sub/deep"),
        catalog_entry("pipelines/triage"),
        catalog_entry("pipelines/plan"),
    ]
}

#[test]
fn catalog_rows_groups_folders_and_tasks() {
    let mut app = App::new();
    app.set_catalog(nested_catalog());
    assert_eq!(
        app.catalog_rows(),
        vec![
            CatalogRow::Task { index: 0, depth: 0 },
            CatalogRow::Folder {
                path: "pipelines".to_string(),
                depth: 0,
                collapsed: false,
            },
            CatalogRow::Task { index: 3, depth: 1 },
            CatalogRow::Folder {
                path: "pipelines/sub".to_string(),
                depth: 1,
                collapsed: false,
            },
            CatalogRow::Task { index: 1, depth: 2 },
            CatalogRow::Task { index: 2, depth: 1 },
        ]
    );
}

#[test]
fn collapsed_folder_hides_descendants_and_nested_folders() {
    let mut app = App::new();
    app.set_catalog(nested_catalog());

    // Collapsing the nested folder keeps its own row but hides its task.
    app.catalog_collapsed.insert("pipelines/sub".to_string());
    let rows = app.catalog_rows();
    assert!(rows.contains(&CatalogRow::Folder {
        path: "pipelines/sub".to_string(),
        depth: 1,
        collapsed: true,
    }));
    assert!(!rows
        .iter()
        .any(|r| matches!(r, CatalogRow::Task { index, .. } if *index == 1)));

    // Collapsing the parent hides the nested folder too.
    app.catalog_collapsed.insert("pipelines".to_string());
    assert_eq!(
        app.catalog_rows(),
        vec![
            CatalogRow::Task { index: 0, depth: 0 },
            CatalogRow::Folder {
                path: "pipelines".to_string(),
                depth: 0,
                collapsed: true,
            },
        ]
    );
}

#[test]
fn enter_on_folder_toggles_and_never_starts() {
    let mut app = App::new();
    app.tab = Tab::Catalog;
    app.set_catalog(vec![catalog_entry("pipelines/plan")]);
    assert_eq!(app.catalog_selected, 0);

    // Selected row is the folder: Enter folds, it never starts a task.
    assert!(matches!(
        app.handle_key(key(KeyCode::Enter, KeyModifiers::empty())),
        UiAction::None
    ));
    assert!(app.catalog_collapsed.contains("pipelines"));

    // A second Enter unfolds it.
    assert!(matches!(
        app.handle_key(key(KeyCode::Enter, KeyModifiers::empty())),
        UiAction::None
    ));
    assert!(!app.catalog_collapsed.contains("pipelines"));

    // On the task row, Enter starts the folder-qualified task.
    app.select_catalog_row(1);
    match app.handle_key(key(KeyCode::Enter, KeyModifiers::empty())) {
        UiAction::StartTask(name) => assert_eq!(name, "pipelines/plan"),
        _ => panic!("expected StartTask"),
    }
}

#[test]
fn space_toggles_selected_folder_only() {
    let mut app = App::new();
    app.tab = Tab::Catalog;
    app.set_catalog(vec![catalog_entry("pipelines/plan")]);

    app.handle_key(key(KeyCode::Char(' '), KeyModifiers::empty()));
    assert!(app.catalog_collapsed.contains("pipelines"));
    app.handle_key(key(KeyCode::Char(' '), KeyModifiers::empty()));
    assert!(!app.catalog_collapsed.contains("pipelines"));

    // On a task row Space is a no-op.
    app.select_catalog_row(1);
    app.handle_key(key(KeyCode::Char(' '), KeyModifiers::empty()));
    assert!(app.catalog_collapsed.is_empty());
}

#[test]
fn catalog_navigation_walks_folder_and_task_rows() {
    let mut app = App::new();
    app.tab = Tab::Catalog;
    app.set_catalog(nested_catalog());
    assert_eq!(app.catalog_selected, 0);

    for expected in [1, 2, 3, 4, 5, 5] {
        app.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
        assert_eq!(app.catalog_selected, expected);
    }
    for expected in [4, 3, 2, 1, 0, 0] {
        app.handle_key(key(KeyCode::Up, KeyModifiers::empty()));
        assert_eq!(app.catalog_selected, expected);
    }

    // Selecting a folder clears the (stale) preview so the pane shows its
    // placeholder instead of the previously highlighted task.
    app.catalog_preview = Some(("pipelines/triage".to_string(), "md".to_string()));
    app.select_catalog_row(1); // the `pipelines` folder
    assert!(app.catalog_preview.is_none());
}

#[test]
fn set_catalog_clamps_selection_against_visible_rows() {
    let mut app = App::new();
    app.catalog_selected = 99;
    app.set_catalog(vec![catalog_entry("a"), catalog_entry("pipelines/plan")]);
    // Three visible rows (`a`, `pipelines` folder, `pipelines/plan`) even
    // though the catalog holds two entries.
    assert_eq!(app.catalog_rows().len(), 3);
    assert_eq!(app.catalog_selected, 2);
}

#[test]
fn catalog_preview_target_is_none_on_folder_row() {
    let mut app = App::new();
    app.tab = Tab::Catalog;
    app.set_catalog(vec![catalog_entry("pipelines/plan")]);

    app.select_catalog_row(0); // folder
    assert_eq!(app.catalog_preview_target(), None);

    app.select_catalog_row(1); // task
    assert_eq!(
        app.catalog_preview_target().as_deref(),
        Some("pipelines/plan")
    );
}

#[test]
fn clicking_catalog_folder_toggles_it() {
    let mut app = App::new();
    app.tab = Tab::Catalog;
    app.set_catalog(vec![catalog_entry("pipelines/plan")]);
    let inner = Rect {
        x: 0,
        y: 2,
        width: 20,
        height: 5,
    };
    app.catalog_geom = geom(inner, 0, 2);

    // First click on the folder row (index 0) folds it.
    assert!(matches!(app.handle_mouse(click(5, 2)), UiAction::None));
    assert!(app.catalog_collapsed.contains("pipelines"));

    // A second click unfolds it.
    assert!(matches!(app.handle_mouse(click(5, 2)), UiAction::None));
    assert!(!app.catalog_collapsed.contains("pipelines"));
}

#[test]
fn encode_mouse_sgr_reports_left_press() {
    let mut parser = vt100::Parser::new(24, 80, 0);
    parser.process(b"\x1b[?1000h\x1b[?1006h");
    let area = Rect {
        x: 2,
        y: 1,
        width: 80,
        height: 24,
    };
    let ev = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 7,
        row: 4,
        modifiers: KeyModifiers::empty(),
    };
    let bytes = encode_mouse(ev, area, parser.screen()).unwrap();
    assert_eq!(String::from_utf8(bytes).unwrap(), "\x1b[<0;6;4M");
}

#[test]
fn encode_mouse_is_ignored_when_agent_disabled_reporting() {
    let parser = vt100::Parser::new(24, 80, 0);
    let area = Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 24,
    };
    let ev = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 1,
        row: 1,
        modifiers: KeyModifiers::empty(),
    };
    assert!(encode_mouse(ev, area, parser.screen()).is_none());
}

#[test]
fn mouse_mode_survives_daemon_formatted_frames() {
    // The daemon streams `state_formatted` frames (screen contents plus input
    // modes); the client parses them, so the agent's enabled mouse mode must be
    // preserved across the wire.
    let mut daemon = vt100::Parser::new(24, 80, 0);
    daemon.process(b"\x1b[?1000h\x1b[?1006h");
    let frame = daemon.screen().state_formatted();
    let mut client = vt100::Parser::new(24, 80, 0);
    client.process(&frame);
    assert_ne!(
        client.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    assert_eq!(
        client.screen().mouse_protocol_encoding(),
        vt100::MouseProtocolEncoding::Sgr
    );
}

fn agent_entry(name: &str) -> AgentCatalogEntry {
    AgentCatalogEntry {
        name: name.to_string(),
        display_name: name.to_string(),
        command: "opencode".to_string(),
        default: false,
        available: true,
        capabilities: AgentCapabilities {
            interactive: true,
            providers: true,
            model_selection: true,
            ..Default::default()
        },
        sessions: Vec::new(),
    }
}

#[test]
fn wizard_skips_provider_model_for_narrow_agent() {
    let mut app = App::new();
    app.daemon_cwd = Some("/code/che".to_string());
    let narrow = AgentCatalogEntry {
        name: "pi".to_string(),
        display_name: "Pi".to_string(),
        command: "pi".to_string(),
        default: false,
        available: true,
        capabilities: AgentCapabilities {
            interactive: true,
            ..Default::default()
        },
        sessions: Vec::new(),
    };
    app.open_wizard(vec![narrow]);

    // A narrow agent advances straight from Agent to Dir; no provider fetch.
    assert!(matches!(
        app.handle_key(key(KeyCode::Enter, KeyModifiers::empty())),
        UiAction::None
    ));
    {
        let Popup::Wizard(w) = &app.popup else {
            panic!("wizard")
        };
        assert_eq!(w.step, WizardStep::Dir);
        assert_eq!(w.agent.as_deref(), Some("pi"));
    }

    match app.handle_key(key(KeyCode::Enter, KeyModifiers::empty())) {
        UiAction::WizardStart {
            agent,
            provider,
            model,
            cwd,
        } => {
            assert_eq!(agent, "pi");
            assert!(provider.is_none());
            assert!(model.is_none());
            assert_eq!(cwd.as_deref(), Some("/code/che"));
        }
        _ => panic!("expected WizardStart"),
    }
}

#[test]
fn wizard_walks_agent_provider_model_dir() {
    let mut app = App::new();
    app.daemon_cwd = Some("/code/che".to_string());
    app.open_wizard(vec![agent_entry("opencode")]);

    // Agent step -> request the provider catalog.
    assert!(matches!(
        app.handle_key(key(KeyCode::Enter, KeyModifiers::empty())),
        UiAction::WizardLoadProviders
    ));

    // Catalog arrives; the provider step lists display names.
    app.wizard_set_providers(vec![
        Provider {
            id: "deepseek".to_string(),
            name: "DeepSeek".to_string(),
            models: vec![
                favetto_providers::Model {
                    id: "deepseek-v4-flash".to_string(),
                    name: "DeepSeek V4 Flash".to_string(),
                },
                favetto_providers::Model {
                    id: "deepseek-v4-pro".to_string(),
                    name: "DeepSeek V4 Pro".to_string(),
                },
            ],
        },
        Provider {
            id: "mistral".to_string(),
            name: "Mistral".to_string(),
            models: vec![favetto_providers::Model {
                id: "mistral-large".to_string(),
                name: "Mistral Large".to_string(),
            }],
        },
    ]);
    {
        let Popup::Wizard(w) = &app.popup else {
            panic!("wizard")
        };
        assert_eq!(w.step, WizardStep::Provider);
        assert_eq!(
            w.choices,
            vec![
                ("DeepSeek".to_string(), "deepseek".to_string()),
                ("Mistral".to_string(), "mistral".to_string()),
            ]
        );
    }

    // Pick DeepSeek -> model step lists its models by display name.
    app.handle_key(key(KeyCode::Enter, KeyModifiers::empty()));
    {
        let Popup::Wizard(w) = &app.popup else {
            panic!("wizard")
        };
        assert_eq!(w.step, WizardStep::Model);
        assert_eq!(
            w.choices[0],
            (
                "DeepSeek V4 Flash".to_string(),
                "deepseek-v4-flash".to_string()
            )
        );
    }

    // Pick the first model -> directory step.
    app.handle_key(key(KeyCode::Enter, KeyModifiers::empty()));
    {
        let Popup::Wizard(w) = &app.popup else {
            panic!("wizard")
        };
        assert_eq!(w.step, WizardStep::Dir);
        assert_eq!(w.model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(w.dir, "/code/che");
    }

    match app.handle_key(key(KeyCode::Enter, KeyModifiers::empty())) {
        UiAction::WizardStart {
            agent,
            provider,
            model,
            cwd,
        } => {
            assert_eq!(agent, "opencode");
            assert_eq!(provider.as_deref(), Some("deepseek"));
            assert_eq!(model.as_deref(), Some("deepseek-v4-flash"));
            assert_eq!(cwd.as_deref(), Some("/code/che"));
        }
        _ => panic!("expected WizardStart"),
    }
    assert!(matches!(app.popup, Popup::None));
}

#[test]
fn row_index_at_maps_visible_rows_with_offset() {
    let inner = Rect {
        x: 0,
        y: 3,
        width: 20,
        height: 5,
    };
    assert_eq!(row_index_at(inner, 0, 10, 3, 0), Some(0));
    assert_eq!(row_index_at(inner, 0, 10, 7, 0), Some(4));
    assert_eq!(row_index_at(inner, 4, 10, 3, 0), Some(4));
    assert_eq!(row_index_at(inner, 4, 10, 7, 0), Some(8));
    // Offset pushes the index past `len`.
    assert_eq!(row_index_at(inner, 8, 10, 5, 0), None);
    // Empty table.
    assert_eq!(row_index_at(inner, 0, 0, 3, 0), None);
    // Last visible row is past a short list.
    assert_eq!(row_index_at(inner, 0, 3, 7, 0), None);
    // Column outside the data area (border/header rows are outside `inner`).
    assert_eq!(row_index_at(inner, 0, 10, 3, 20), None);
    assert_eq!(row_index_at(inner, 0, 10, 2, 0), None);
}

#[test]
fn table_rows_area_offsets_border_and_header() {
    let area = Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 24,
    };
    assert_eq!(
        table_rows_area(area),
        Rect {
            x: 1,
            y: 2,
            width: 78,
            height: 21,
        }
    );
}

#[test]
fn clicking_tasks_row_selects_then_opens_agent() {
    let mut app = App::new();
    app.tab = Tab::Tasks;
    let tasks = vec![task("a"), task("b")];
    let second_id = tasks[1].id.to_string();
    app.tasks = tasks;
    app.tasks_selected = 0;
    let inner = Rect {
        x: 0,
        y: 2,
        width: 20,
        height: 5,
    };
    app.tasks_geom = geom(inner, 0, 2);

    // First click selects index 1 (no activation).
    assert!(matches!(app.handle_mouse(click(5, 3)), UiAction::None));
    assert_eq!(app.tasks_selected, 1);

    // Second click on the now-selected row opens its agent.
    match app.handle_mouse(click(5, 3)) {
        UiAction::OpenAgent(id) => assert_eq!(id, second_id),
        _ => panic!("expected OpenAgent"),
    }
}

#[test]
fn clicking_catalog_row_selects_resets_scroll_then_starts_task() {
    let mut app = App::new();
    app.tab = Tab::Catalog;
    app.catalog = vec![catalog_entry("a"), catalog_entry("b")];
    app.catalog_selected = 1;
    app.catalog_preview_scroll = 7;
    let inner = Rect {
        x: 0,
        y: 2,
        width: 20,
        height: 5,
    };
    app.catalog_geom = geom(inner, 0, 2);

    assert!(matches!(app.handle_mouse(click(5, 2)), UiAction::None));
    assert_eq!(app.catalog_selected, 0);
    assert_eq!(app.catalog_preview_scroll, 0);

    match app.handle_mouse(click(5, 2)) {
        UiAction::StartTask(name) => assert_eq!(name, "a"),
        _ => panic!("expected StartTask"),
    }
}

#[test]
fn clicking_events_row_selects_newest_first() {
    let mut app = App::new();
    app.tab = Tab::Events;
    for id in 1..=2 {
        app.ingest_event(Event {
            id,
            kind: EventKind::Unknown,
            payload: serde_json::json!({}),
            created_at: Utc::now(),
        });
    }
    let inner = Rect {
        x: 0,
        y: 2,
        width: 20,
        height: 5,
    };
    app.events_geom = geom(inner, 0, 2);

    assert!(matches!(app.handle_mouse(click(5, 2)), UiAction::None));
    assert_eq!(app.events_selected, 0);
    assert_eq!(app.selected_event().map(|e| e.id), Some(2));

    assert!(matches!(app.handle_mouse(click(5, 3)), UiAction::None));
    assert_eq!(app.events_selected, 1);
    assert_eq!(app.selected_event().map(|e| e.id), Some(1));
}

#[test]
fn clicking_scheduler_and_notifications_rows_selects_only() {
    let mut app = App::new();
    app.schedules = vec![Schedule {
        id: "s1".to_string(),
        cron: "0 * * * *".to_string(),
        task: "t".to_string(),
        input: serde_json::json!({}),
        enabled: true,
        last_run: None,
    }];
    app.notifications = vec![NotificationRecord {
        id: 1,
        channel: "cli".to_string(),
        subject: "hi".to_string(),
        body: "body".to_string(),
        status: "ok".to_string(),
        sent_at: Utc::now(),
    }];
    let inner = Rect {
        x: 0,
        y: 2,
        width: 20,
        height: 5,
    };

    app.tab = Tab::Scheduler;
    app.schedules_geom = geom(inner, 0, 1);
    assert!(matches!(app.handle_mouse(click(5, 2)), UiAction::None));
    assert_eq!(app.schedules_selected, 0);
    assert!(matches!(app.handle_mouse(click(5, 2)), UiAction::None));

    app.tab = Tab::Notifications;
    app.notifications_geom = geom(inner, 0, 1);
    assert!(matches!(app.handle_mouse(click(5, 2)), UiAction::None));
    assert_eq!(app.notifications_selected, 0);
    assert!(matches!(app.handle_mouse(click(5, 2)), UiAction::None));
}

#[test]
fn click_is_ignored_while_popup_open() {
    let mut app = App::new();
    app.tab = Tab::Tasks;
    app.tasks = vec![task("a"), task("b")];
    app.tasks_selected = 0;
    let inner = Rect {
        x: 0,
        y: 2,
        width: 20,
        height: 5,
    };
    app.tasks_geom = geom(inner, 0, 2);
    app.popup = Popup::Menu { selected: 0 };

    assert!(matches!(app.handle_mouse(click(5, 3)), UiAction::None));
    assert_eq!(app.tasks_selected, 0);
}

#[test]
fn wheel_over_list_moves_selection() {
    let mut app = App::new();
    app.tab = Tab::Tasks;
    app.tasks = (0..10).map(|i| task(&format!("t{i}"))).collect();
    let inner = Rect {
        x: 0,
        y: 2,
        width: 20,
        height: 5,
    };
    app.tasks_geom = geom(inner, 0, 10);

    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 5,
        row: 3,
        modifiers: KeyModifiers::empty(),
    });
    assert_eq!(app.tasks_selected, 3);

    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 5,
        row: 3,
        modifiers: KeyModifiers::empty(),
    });
    assert_eq!(app.tasks_selected, 0);

    // The wheel over the Catalog list still resets the preview scroll.
    app.tab = Tab::Catalog;
    app.catalog = vec![catalog_entry("a"), catalog_entry("b")];
    app.catalog_geom = geom(inner, 0, 2);
    app.catalog_preview_scroll = 5;
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 5,
        row: 3,
        modifiers: KeyModifiers::empty(),
    });
    assert_eq!(app.catalog_preview_scroll, 0);
}

#[test]
fn wheel_over_floating_preview_scrolls_preview_not_tree() {
    let mut app = App::new();
    app.tab = Tab::Catalog;
    app.catalog = (0..6).map(|i| catalog_entry(&format!("t{i}"))).collect();
    app.catalog_preview_max_scroll = 100;
    // The floating preview sits inside the full-width tree hit-test rect.
    let tree_inner = Rect {
        x: 0,
        y: 1,
        width: 79,
        height: 28,
    };
    app.catalog_geom = geom(tree_inner, 0, 6);
    app.catalog_selected = 1;
    app.catalog_preview_area = Some(Rect {
        x: 8,
        y: 4,
        width: 60,
        height: 20,
    });

    // Wheel over the overlay: preview scrolls, tree does not move.
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 20,
        row: 10,
        modifiers: KeyModifiers::empty(),
    });
    assert_eq!(app.catalog_preview_scroll, 3);
    assert_eq!(app.catalog_selected, 1, "tree selection must not move");

    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 20,
        row: 10,
        modifiers: KeyModifiers::empty(),
    });
    assert_eq!(app.catalog_preview_scroll, 0);
    assert_eq!(app.catalog_selected, 1);

    // Wheel over the tree, outside the overlay: tree scrolls as before and
    // the selection change resets the preview scroll.
    app.catalog_preview_scroll = 5;
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 2,
        row: 2,
        modifiers: KeyModifiers::empty(),
    });
    assert_eq!(app.catalog_selected, 4);
    assert_eq!(app.catalog_preview_scroll, 0);
}

#[test]
fn wheel_over_hidden_preview_falls_through_to_tree() {
    let mut app = App::new();
    app.tab = Tab::Catalog;
    app.catalog = (0..6).map(|i| catalog_entry(&format!("t{i}"))).collect();
    app.catalog_geom = geom(
        Rect {
            x: 0,
            y: 1,
            width: 79,
            height: 28,
        },
        0,
        6,
    );
    app.catalog_selected = 1;
    // A stale rect plus visible=false must not capture the wheel.
    app.catalog_preview_area = Some(Rect {
        x: 8,
        y: 4,
        width: 60,
        height: 20,
    });
    app.catalog_preview_visible = false;

    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 20,
        row: 10,
        modifiers: KeyModifiers::empty(),
    });
    assert_eq!(app.catalog_selected, 4);
}

#[test]
fn click_on_border_or_below_rows_is_noop() {
    let mut app = App::new();
    app.tab = Tab::Tasks;
    app.tasks = vec![task("a"), task("b")];
    app.tasks_selected = 0;
    let inner = Rect {
        x: 1,
        y: 2,
        width: 20,
        height: 5,
    };
    app.tasks_geom = geom(inner, 0, 2);

    // Left border, header row, and below the rows all leave the selection be.
    for (col, row) in [(0, 3), (1, 1), (1, 7)] {
        assert!(matches!(app.handle_mouse(click(col, row)), UiAction::None));
        assert_eq!(app.tasks_selected, 0);
    }
}

#[test]
fn keyboard_up_down_moves_scheduler_and_notification_selection() {
    let mut app = App::new();
    app.schedules = vec![
        Schedule {
            id: "s1".to_string(),
            cron: "0 * * * *".to_string(),
            task: "t".to_string(),
            input: serde_json::json!({}),
            enabled: true,
            last_run: None,
        },
        Schedule {
            id: "s2".to_string(),
            cron: "0 0 * * *".to_string(),
            task: "u".to_string(),
            input: serde_json::json!({}),
            enabled: false,
            last_run: None,
        },
    ];
    app.notifications = vec![
        NotificationRecord {
            id: 1,
            channel: "cli".to_string(),
            subject: "a".to_string(),
            body: String::new(),
            status: "ok".to_string(),
            sent_at: Utc::now(),
        },
        NotificationRecord {
            id: 2,
            channel: "cli".to_string(),
            subject: "b".to_string(),
            body: String::new(),
            status: "ok".to_string(),
            sent_at: Utc::now(),
        },
    ];

    app.tab = Tab::Scheduler;
    app.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
    assert_eq!(app.schedules_selected, 1);
    app.handle_key(key(KeyCode::Up, KeyModifiers::empty()));
    assert_eq!(app.schedules_selected, 0);

    app.tab = Tab::Notifications;
    app.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
    assert_eq!(app.notifications_selected, 1);
    app.handle_key(key(KeyCode::Up, KeyModifiers::empty()));
    assert_eq!(app.notifications_selected, 0);
}

fn vars_entry(name: &str) -> CatalogEntry {
    CatalogEntry {
        name: name.to_string(),
        agent: None,
        provider: None,
        model: None,
        cwd: None,
        needs: None,
        vars: vec![
            TaskVar {
                name: "description".to_string(),
                prompt: "Describe".to_string(),
                default: Some("draft".to_string()),
                required: true,
                multiline: false,
                var_type: VarType::String,
                choices: None,
            },
            TaskVar {
                name: "count".to_string(),
                prompt: "Count".to_string(),
                default: None,
                required: false,
                multiline: false,
                var_type: VarType::Int,
                choices: None,
            },
        ],
        prompt: String::new(),
    }
}

fn open_vars_form(app: &mut App, entry: CatalogEntry) {
    app.tab = Tab::Catalog;
    app.catalog = vec![entry];
    app.handle_key(key(KeyCode::Enter, KeyModifiers::empty()));
    assert!(matches!(app.popup, Popup::TaskVars(_)));
}

#[test]
fn enter_on_catalog_entry_without_vars_returns_start_task() {
    let mut app = App::new();
    app.tab = Tab::Catalog;
    app.catalog = vec![catalog_entry("a")];
    match app.handle_key(key(KeyCode::Enter, KeyModifiers::empty())) {
        UiAction::StartTask(name) => assert_eq!(name, "a"),
        _ => panic!("expected StartTask"),
    }
    assert!(matches!(app.popup, Popup::None));
}

#[test]
fn enter_on_vars_task_opens_form_with_defaults() {
    let mut app = App::new();
    open_vars_form(&mut app, vars_entry("issue"));
    let Popup::TaskVars(form) = &app.popup else {
        panic!("expected TaskVars");
    };
    assert_eq!(form.task, "issue");
    assert_eq!(
        form.values.iter().map(|v| v.value()).collect::<Vec<_>>(),
        vec!["draft", ""]
    );
    assert_eq!(form.current, 0);
    assert!(form.error.is_none());
}

#[test]
fn required_var_blocks_submit_until_filled() {
    let mut app = App::new();
    open_vars_form(&mut app, vars_entry("issue"));
    for _ in 0.."draft".len() {
        app.handle_key(key(KeyCode::Backspace, KeyModifiers::empty()));
    }
    assert!(matches!(
        app.handle_key(key(KeyCode::Enter, KeyModifiers::CONTROL)),
        UiAction::None
    ));
    match &app.popup {
        Popup::TaskVars(form) => assert!(form.error.is_some()),
        _ => panic!("form should stay open with an error"),
    }

    for c in "hello".chars() {
        app.handle_key(key(KeyCode::Char(c), KeyModifiers::empty()));
    }
    app.handle_key(key(KeyCode::Tab, KeyModifiers::empty()));
    match app.handle_key(key(KeyCode::Enter, KeyModifiers::CONTROL)) {
        UiAction::StartTaskWithInput { name, input } => {
            assert_eq!(name, "issue");
            assert_eq!(input, serde_json::json!({ "description": "hello" }));
        }
        _ => panic!("expected StartTaskWithInput"),
    }
    assert!(matches!(app.popup, Popup::None));
}

#[test]
fn int_var_coerces_and_non_numeric_blocks() {
    let mut app = App::new();
    open_vars_form(&mut app, vars_entry("issue"));
    app.handle_key(key(KeyCode::Tab, KeyModifiers::empty()));
    app.handle_key(key(KeyCode::Char('x'), KeyModifiers::empty()));
    assert!(matches!(
        app.handle_key(key(KeyCode::Enter, KeyModifiers::empty())),
        UiAction::None
    ));
    assert!(matches!(&app.popup, Popup::TaskVars(form) if form.error.is_some()));

    app.handle_key(key(KeyCode::Backspace, KeyModifiers::empty()));
    app.handle_key(key(KeyCode::Char('3'), KeyModifiers::empty()));
    match app.handle_key(key(KeyCode::Enter, KeyModifiers::empty())) {
        UiAction::StartTaskWithInput { input, .. } => {
            assert_eq!(
                input,
                serde_json::json!({ "description": "draft", "count": 3 })
            );
        }
        _ => panic!("expected StartTaskWithInput"),
    }
}

#[test]
fn multiline_enter_inserts_newline_and_ctrl_enter_submits() {
    let mut app = App::new();
    let mut entry = vars_entry("issue");
    entry.vars = vec![TaskVar {
        name: "body".to_string(),
        prompt: "Body".to_string(),
        default: None,
        required: true,
        multiline: true,
        var_type: VarType::String,
        choices: None,
    }];
    open_vars_form(&mut app, entry);
    app.handle_key(key(KeyCode::Char('a'), KeyModifiers::empty()));
    app.handle_key(key(KeyCode::Enter, KeyModifiers::empty()));
    app.handle_key(key(KeyCode::Char('b'), KeyModifiers::empty()));
    match &app.popup {
        Popup::TaskVars(form) => assert_eq!(form.values[0], "a\nb"),
        _ => panic!("form closed on Enter"),
    }
    match app.handle_key(key(KeyCode::Enter, KeyModifiers::CONTROL)) {
        UiAction::StartTaskWithInput { input, .. } => {
            assert_eq!(input, serde_json::json!({ "body": "a\nb" }));
        }
        _ => panic!("expected StartTaskWithInput"),
    }
}

#[test]
fn task_vars_caret_keymap_edits_mid_string() {
    let mut app = App::new();
    let mut entry = vars_entry("issue");
    entry.vars = vec![TaskVar {
        name: "body".to_string(),
        prompt: "Body".to_string(),
        default: Some("hello brave world".to_string()),
        required: false,
        multiline: true,
        var_type: VarType::String,
        choices: None,
    }];
    open_vars_form(&mut app, entry);

    // Alt+Left jumps to the start of the last word, then type and edit.
    app.handle_key(key(KeyCode::Left, KeyModifiers::ALT));
    app.handle_key(key(KeyCode::Char('X'), KeyModifiers::empty()));
    app.handle_key(key(KeyCode::Home, KeyModifiers::empty()));
    app.handle_key(key(KeyCode::Char('Y'), KeyModifiers::empty()));
    app.handle_key(key(KeyCode::End, KeyModifiers::empty()));
    app.handle_key(key(KeyCode::Backspace, KeyModifiers::empty()));
    match &app.popup {
        Popup::TaskVars(form) => {
            assert_eq!(form.values[0].value(), "Yhello brave Xworl");
            assert_eq!(form.values[0].caret(), "Yhello brave Xworl".len());
        }
        _ => panic!("expected TaskVars"),
    }
}

#[test]
fn task_vars_shift_and_alt_enter_insert_newlines() {
    let mut app = App::new();
    let mut entry = vars_entry("issue");
    entry.vars = vec![TaskVar {
        name: "body".to_string(),
        prompt: "Body".to_string(),
        default: Some("a".to_string()),
        required: false,
        multiline: true,
        var_type: VarType::String,
        choices: None,
    }];
    open_vars_form(&mut app, entry);
    app.handle_key(key(KeyCode::Enter, KeyModifiers::SHIFT));
    app.handle_key(key(KeyCode::Char('b'), KeyModifiers::empty()));
    app.handle_key(key(KeyCode::Enter, KeyModifiers::ALT));
    app.handle_key(key(KeyCode::Char('c'), KeyModifiers::empty()));
    match &app.popup {
        Popup::TaskVars(form) => assert_eq!(form.values[0].value(), "a\nb\nc"),
        _ => panic!("expected TaskVars"),
    }
}

#[test]
fn task_vars_multiline_arrows_move_the_caret_between_lines() {
    let mut app = App::new();
    let mut entry = vars_entry("issue");
    entry.vars = vec![TaskVar {
        name: "body".to_string(),
        prompt: "Body".to_string(),
        default: Some("one\ntwo".to_string()),
        required: false,
        multiline: true,
        var_type: VarType::String,
        choices: None,
    }];
    open_vars_form(&mut app, entry);
    // Caret starts at the end of "two"; Up moves to the same column on line 0.
    app.handle_key(key(KeyCode::Up, KeyModifiers::empty()));
    app.handle_key(key(KeyCode::Char('X'), KeyModifiers::empty()));
    match &app.popup {
        Popup::TaskVars(form) => {
            assert_eq!(form.values[0].value(), "oneX\ntwo");
            assert_eq!(form.current, 0, "Up moves the caret, not the field");
        }
        _ => panic!("expected TaskVars"),
    }
}

#[test]
fn task_vars_single_line_arrows_still_change_fields() {
    let mut app = App::new();
    open_vars_form(&mut app, vars_entry("issue"));
    app.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
    match &app.popup {
        Popup::TaskVars(form) => assert_eq!(form.current, 1),
        _ => panic!("expected TaskVars"),
    }
}

#[test]
fn form_caret_keymap_edits_mid_string() {
    let mut app = App::new();
    app.handle_key(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
    app.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
    app.handle_key(key(KeyCode::Enter, KeyModifiers::empty()));
    for c in "abcd".chars() {
        app.handle_key(key(KeyCode::Char(c), KeyModifiers::empty()));
    }
    // Alt+Left jumps to the start of the word, Home/End go to line bounds.
    app.handle_key(key(KeyCode::Left, KeyModifiers::ALT));
    app.handle_key(key(KeyCode::Char('X'), KeyModifiers::empty()));
    app.handle_key(key(KeyCode::Home, KeyModifiers::empty()));
    app.handle_key(key(KeyCode::Char('Y'), KeyModifiers::empty()));
    app.handle_key(key(KeyCode::End, KeyModifiers::empty()));
    app.handle_key(key(KeyCode::Backspace, KeyModifiers::empty()));
    let Popup::Form(form) = &app.popup else {
        panic!("expected form");
    };
    assert_eq!(form.input.value(), "YXabc");
    assert_eq!(form.input.caret(), "YXabc".len());
}

#[test]
fn wizard_directory_caret_keymap_edits_mid_string() {
    let mut app = App::new();
    app.daemon_cwd = Some("/code/che".to_string());
    let narrow = AgentCatalogEntry {
        name: "pi".to_string(),
        display_name: "Pi".to_string(),
        command: "pi".to_string(),
        default: false,
        available: true,
        capabilities: AgentCapabilities {
            interactive: true,
            ..Default::default()
        },
        sessions: Vec::new(),
    };
    app.open_wizard(vec![narrow]);
    app.handle_key(key(KeyCode::Enter, KeyModifiers::empty()));

    // Alt+Left jumps to the start of "che", then insert at the caret.
    app.handle_key(key(KeyCode::Left, KeyModifiers::ALT));
    app.handle_key(key(KeyCode::Char('X'), KeyModifiers::empty()));
    app.handle_key(key(KeyCode::Home, KeyModifiers::empty()));
    app.handle_key(key(KeyCode::Char('Y'), KeyModifiers::empty()));
    app.handle_key(key(KeyCode::End, KeyModifiers::empty()));
    app.handle_key(key(KeyCode::Backspace, KeyModifiers::empty()));
    match &app.popup {
        Popup::Wizard(w) => assert_eq!(w.dir.value(), "Y/code/Xch"),
        _ => panic!("expected wizard"),
    }
    match app.handle_key(key(KeyCode::Enter, KeyModifiers::empty())) {
        UiAction::WizardStart { cwd, .. } => assert_eq!(cwd.as_deref(), Some("Y/code/Xch")),
        _ => panic!("expected WizardStart"),
    }
}

#[test]
fn esc_cancels_var_form_without_starting() {
    let mut app = App::new();
    open_vars_form(&mut app, vars_entry("issue"));
    assert!(matches!(
        app.handle_key(key(KeyCode::Esc, KeyModifiers::empty())),
        UiAction::None
    ));
    assert!(matches!(app.popup, Popup::None));
}

#[test]
fn choices_arrows_change_selection_and_submit_selected() {
    let mut app = App::new();
    let mut entry = vars_entry("issue");
    entry.vars = vec![TaskVar {
        name: "flavor".to_string(),
        prompt: "Flavor".to_string(),
        default: None,
        required: false,
        multiline: false,
        var_type: VarType::String,
        choices: Some(vec!["vanilla".to_string(), "mint".to_string()]),
    }];
    open_vars_form(&mut app, entry);
    app.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
    match &app.popup {
        Popup::TaskVars(form) => {
            assert_eq!(form.choice_selected, vec![1]);
            assert_eq!(
                form.current, 0,
                "Down must change the choice, not the field"
            );
        }
        _ => panic!("expected TaskVars"),
    }
    match app.handle_key(key(KeyCode::Enter, KeyModifiers::empty())) {
        UiAction::StartTaskWithInput { input, .. } => {
            assert_eq!(input, serde_json::json!({ "flavor": "mint" }));
        }
        _ => panic!("expected StartTaskWithInput"),
    }
}

#[test]
fn clicking_vars_catalog_row_opens_form() {
    let mut app = App::new();
    app.tab = Tab::Catalog;
    app.catalog = vec![vars_entry("issue")];
    let inner = Rect {
        x: 0,
        y: 2,
        width: 20,
        height: 5,
    };
    app.catalog_geom = geom(inner, 0, 1);

    // First click selects the row; the second opens the variable form.
    assert!(matches!(app.handle_mouse(click(5, 2)), UiAction::None));
    assert!(matches!(app.handle_mouse(click(5, 2)), UiAction::None));
    assert!(matches!(app.popup, Popup::TaskVars(_)));
}

#[test]
fn needs_animation_tracks_activity() {
    let mut app = App::new();
    app.conn = ConnState::Connected;
    assert!(!app.needs_animation());

    // Reconnecting always animates.
    app.conn = ConnState::Disconnected;
    assert!(app.needs_animation());

    // A running task animates.
    app.conn = ConnState::Connected;
    let mut running = task("r");
    running.status = TaskStatus::Running;
    app.tasks = vec![running];
    assert!(app.needs_animation());

    // So does a task awaiting input.
    let mut awaiting = task("a");
    awaiting.status = TaskStatus::AwaitingInput;
    app.tasks = vec![awaiting];
    assert!(app.needs_animation());

    // A running agent animates.
    app.tasks.clear();
    app.agent_running = true;
    assert!(app.needs_animation());

    // A loading wizard animates, then settles.
    app.agent_running = false;
    app.open_wizard(vec![agent_entry("opencode")]);
    if let Popup::Wizard(w) = &mut app.popup {
        w.loading = true;
    }
    assert!(app.needs_animation());
    if let Popup::Wizard(w) = &mut app.popup {
        w.loading = false;
    }
    assert!(!app.needs_animation());
}

#[test]
fn wizard_lists_unavailable_agent_but_skips_it() {
    let mut app = App::new();
    let mut unavailable = agent_entry("claude");
    unavailable.available = false;
    app.open_wizard(vec![agent_entry("opencode"), unavailable]);

    {
        let Popup::Wizard(w) = &app.popup else {
            panic!("wizard")
        };
        assert_eq!(
            w.choices,
            vec![
                ("opencode".to_string(), "opencode".to_string()),
                ("claude".to_string(), "claude".to_string()),
            ]
        );
        assert_eq!(w.available.get("claude"), Some(&false));
        assert_eq!(w.available.get("opencode"), Some(&true));
        assert_eq!(w.selected, 0);
    }

    // Down skips the unavailable `claude` entry and stays on `opencode`.
    app.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
    {
        let Popup::Wizard(w) = &app.popup else {
            panic!("wizard")
        };
        assert_eq!(w.selected, 0);
    }

    // Even a forced selection on the unavailable entry cannot be entered.
    if let Popup::Wizard(w) = &mut app.popup {
        w.selected = 1;
    }
    assert!(matches!(
        app.handle_key(key(KeyCode::Enter, KeyModifiers::empty())),
        UiAction::None
    ));
    {
        let Popup::Wizard(w) = &app.popup else {
            panic!("wizard")
        };
        assert!(w.agent.is_none());
        assert_eq!(w.step, WizardStep::Agent);
        assert!(w.error.is_some());
    }
}
