use super::*;
use crate::agents::{AgentManager, AgentRegistry};
use crate::config::FavettoConfig;
use crate::event_bus::EventBus;
use crate::state::StateInit;
use crate::tasks::{TaskVar, VarType};
use crate::webhooks::WebhookSecrets;
use favetto_core::auth::Token;
use parking_lot::RwLock;

fn def_with_vars() -> TaskDef {
    let var = |name: &str, required: bool| TaskVar {
        name: name.to_string(),
        prompt: "p".to_string(),
        default: None,
        required,
        multiline: false,
        var_type: VarType::String,
        choices: None,
    };
    TaskDef {
        name: "t".to_string(),
        agent: None,
        provider: None,
        model: None,
        cwd: None,
        schedule: None,
        needs: None,
        spawn: None,
        spawn_file: None,
        spawn_new_root: false,
        sign: None,
        vars: vec![var("required_one", true), var("optional_one", false)],
        prompt: "hello".to_string(),
    }
}

#[test]
fn missing_required_vars_detects_absent_null_and_empty() {
    let def = def_with_vars();
    assert_eq!(
        missing_required_vars(&def, &serde_json::json!({})),
        ["required_one"]
    );
    assert_eq!(
        missing_required_vars(&def, &serde_json::json!({ "required_one": null })),
        ["required_one"]
    );
    assert_eq!(
        missing_required_vars(&def, &serde_json::json!({ "required_one": "" })),
        ["required_one"]
    );
    // Present values (including typed ones) satisfy the requirement.
    assert!(missing_required_vars(&def, &serde_json::json!({ "required_one": "x" })).is_empty());
    assert!(missing_required_vars(&def, &serde_json::json!({ "required_one": 3 })).is_empty());
    // Optional vars never appear.
    assert!(missing_required_vars(&def, &serde_json::json!({ "required_one": "x" })).is_empty());
}

#[test]
fn worktree_root_expands_tilde_and_resolves_relative() {
    let home = dirs::home_dir().expect("home directory");
    let data_dir = Path::new("/data");
    let repo = Path::new("/repo");
    let root_for = |dir: Option<&str>| {
        let cfg = ExecutorSettings {
            worktree_dir: dir.map(PathBuf::from),
            ..Default::default()
        };
        worktree_root(&cfg, data_dir, repo)
    };

    // `~` expands to the home directory before the absolute/relative decision,
    // so it is never joined onto the repo root.
    assert_eq!(
        root_for(Some("~/.local/share/favetto/worktrees")),
        home.join(".local/share/favetto/worktrees")
    );

    // A non-`~` relative value stays repo-relative.
    assert_eq!(
        root_for(Some(".favetto/worktrees")),
        Path::new("/repo/.favetto/worktrees")
    );

    // An absolute value is used as-is.
    assert_eq!(
        root_for(Some("/abs/worktrees")),
        Path::new("/abs/worktrees")
    );

    // Unset falls back to `<data_dir>/worktrees`.
    assert_eq!(root_for(None), Path::new("/data/worktrees"));
}

#[test]
fn render_context_exposes_task_input_and_prev() {
    let task = Task {
        id: Uuid::new_v4(),
        name: "triage".to_string(),
        status: TaskStatus::Succeeded,
        input: serde_json::json!({
            "issue_id": 7,
            "_prev": { "output": "done", "name": "triage" },
        }),
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
    };
    let ctx = render_context(&task);
    assert_eq!(crate::template::render("{{ task.name }}", &ctx), "triage");
    assert_eq!(crate::template::render("{{ input.issue_id }}", &ctx), "7");
    assert_eq!(crate::template::render("{{ prev.output }}", &ctx), "done");
}

#[tokio::test]
async fn unavailable_agent_fails_with_clear_error() {
    let dir = std::env::temp_dir().join(format!("favetto-executor-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let pool = crate::db::open(&dir.join("test.db")).await.unwrap();
    crate::db::migrate(&pool).await.unwrap();

    let mut cfg = FavettoConfig::default();
    cfg.agent.default = Some("opencode".to_string());
    let registry = AgentRegistry::from_config_with(&cfg, &|_| false).unwrap();

    let state = Arc::new(State::new(StateInit {
        db: pool,
        bus: EventBus::new(64),
        token: Token::generate(),
        webhooks: WebhookSecrets {
            github: None,
            linear: None,
        },
        agents: AgentManager::new(),
        registry,
        config: Arc::new(cfg),
        data_dir: dir.clone(),
        tasks_dir: dir.clone(),
        catalog: Arc::new(RwLock::new(Vec::new())),
        scheduler: tokio_cron_scheduler::JobScheduler::new().await.unwrap(),
        hook_store: Arc::new(RwLock::new(Vec::new())),
    }));

    let task = Task {
        id: Uuid::new_v4(),
        name: "oneshot".to_string(),
        status: TaskStatus::Running,
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
    };
    let def = def_with_vars();
    let err = run_agent_task(&state, &task, "opencode", &def, "prompt", Path::new("/tmp"))
        .await
        .map(|_| ())
        .unwrap_err()
        .to_string();
    assert!(err.contains("agent 'opencode' is not installed"), "{err}");
    assert!(err.contains("not found on PATH"), "{err}");
}

fn task_with_session(session_id: Option<&str>, session_title: Option<&str>) -> Task {
    Task {
        id: Uuid::new_v4(),
        name: "t".to_string(),
        status: TaskStatus::Running,
        input: serde_json::json!({}),
        output: None,
        dedupe_key: None,
        created_at: Utc::now(),
        started_at: None,
        finished_at: None,
        error: None,
        session_id: session_id.map(str::to_string),
        session_title: session_title.map(str::to_string),
        parent_id: None,
        root_id: None,
    }
}

#[test]
fn failed_run_preserves_session_info() {
    let mut task = task_with_session(None, None);
    let success = record_run_outcome(
        &mut task,
        Ok(RunOutcome {
            output: serde_json::json!({}),
            session_id: Some("ses_1".to_string()),
            session_title: Some("Fix the widget".to_string()),
            error: Some("exit 1".to_string()),
        }),
    );
    assert!(!success);
    assert_eq!(task.status, TaskStatus::Failed);
    assert_eq!(task.session_id.as_deref(), Some("ses_1"));
    assert_eq!(task.session_title.as_deref(), Some("Fix the widget"));
    assert!(task.error.as_deref().unwrap().contains("exit 1"));
    assert!(task.output.is_none());
}

#[test]
fn successful_run_records_output_and_session() {
    let mut task = task_with_session(None, None);
    task.error = Some("stale".to_string());
    let success = record_run_outcome(
        &mut task,
        Ok(RunOutcome {
            output: serde_json::json!({ "ok": true }),
            session_id: Some("ses_1".to_string()),
            session_title: Some("Fix the widget".to_string()),
            error: None,
        }),
    );
    assert!(success);
    assert_eq!(task.status, TaskStatus::Succeeded);
    assert_eq!(task.output, Some(serde_json::json!({ "ok": true })));
    assert_eq!(task.session_id.as_deref(), Some("ses_1"));
    assert_eq!(task.session_title.as_deref(), Some("Fix the widget"));
    assert!(task.error.is_none());
}

#[test]
fn run_without_agent_still_fails() {
    let mut task = task_with_session(Some("ses_keep"), Some("Keep"));
    let success = record_run_outcome(&mut task, Err(anyhow::anyhow!("no agent")));
    assert!(!success);
    assert_eq!(task.status, TaskStatus::Failed);
    // A pre-run failure never drops an already-resolved session.
    assert_eq!(task.session_id.as_deref(), Some("ses_keep"));
    assert_eq!(task.session_title.as_deref(), Some("Keep"));
    assert!(task.error.as_deref().unwrap().contains("no agent"));
}

#[test]
fn terminal_outcome_overrides_awaiting_input_status() {
    let mut task = task_with_session(None, None);
    task.status = TaskStatus::AwaitingInput;
    let success = record_run_outcome(
        &mut task,
        Ok(RunOutcome {
            output: serde_json::json!({}),
            session_id: None,
            session_title: None,
            error: None,
        }),
    );
    assert!(success);
    assert_eq!(task.status, TaskStatus::Succeeded);

    let mut failed = task_with_session(None, None);
    failed.status = TaskStatus::AwaitingInput;
    let success = record_run_outcome(
        &mut failed,
        Ok(RunOutcome {
            output: serde_json::json!({}),
            session_id: None,
            session_title: None,
            error: Some("exit 1".to_string()),
        }),
    );
    assert!(!success);
    assert_eq!(failed.status, TaskStatus::Failed);
}

/// A fake `opencode session list --format json` executable that ignores its
/// arguments and prints `json`.
#[cfg(unix)]
fn session_list_script(dir: &Path, json: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("opencode-session-list.sh");
    std::fs::write(&path, format!("#!/bin/sh\nprintf '%s' '{json}'\n")).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

/// A fake `opencode` that reports a live session id and keeps running unless
/// invoked as `session list`, where it prints `title_json`.
#[cfg(unix)]
fn live_session_script(dir: &Path, title_json: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("opencode-live.sh");
    let script = format!(
            "#!/bin/sh\ncase \"$*\" in\n  *\"session list\"*) printf '%s\\n' '{title_json}' ;;\n  *) printf '%s\\n' '{{\"sessionID\":\"ses_live\"}}' ; sleep 30 ;;\nesac\n"
        );
    std::fs::write(&path, script).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

/// A `State` whose `opencode` command is `command` (a title-lookup fixture).
#[cfg(unix)]
async fn title_state(dir: &Path, command: &Path) -> Arc<State> {
    let pool = crate::db::open(&dir.join("test.db")).await.unwrap();
    crate::db::migrate(&pool).await.unwrap();
    let mut cfg = FavettoConfig::default();
    cfg.agent.default = Some("opencode".to_string());
    cfg.agents.insert(
        "opencode".to_string(),
        crate::config::AgentConfig {
            command: command.to_string_lossy().into_owned(),
            ..Default::default()
        },
    );
    let registry = AgentRegistry::from_config_with(&cfg, &|_| true).unwrap();
    Arc::new(State::new(StateInit {
        db: pool,
        bus: EventBus::new(64),
        token: Token::generate(),
        webhooks: WebhookSecrets {
            github: None,
            linear: None,
        },
        agents: AgentManager::new(),
        registry,
        config: Arc::new(cfg),
        data_dir: dir.to_path_buf(),
        tasks_dir: dir.to_path_buf(),
        catalog: Arc::new(RwLock::new(Vec::new())),
        scheduler: tokio_cron_scheduler::JobScheduler::new().await.unwrap(),
        hook_store: Arc::new(RwLock::new(Vec::new())),
    }))
}

#[cfg(unix)]
#[tokio::test]
async fn backfill_title_fills_blank_session_title() {
    let dir = std::env::temp_dir().join(format!("favetto-backfill-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = session_list_script(&dir, r#"[{"id":"ses_1","title":"One shot title"}]"#);
    let state = title_state(&dir, &script).await;

    let task = task_with_session(Some("ses_1"), None);
    db::insert_task(&state.db, &task).await.unwrap();

    let agent = state.registry.get("opencode").unwrap();
    backfill_title(
        &state,
        task.id,
        agent,
        "ses_1".to_string(),
        dir.clone(),
        3,
        Duration::from_millis(1),
    )
    .await;

    let stored = db::get_task(&state.db, task.id).await.unwrap().unwrap();
    assert_eq!(stored.session_title.as_deref(), Some("One shot title"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[tokio::test]
async fn backfill_title_does_not_overwrite_existing_title() {
    let dir = std::env::temp_dir().join(format!("favetto-backfill-keep-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = session_list_script(&dir, r#"[{"id":"ses_1","title":"One shot title"}]"#);
    let state = title_state(&dir, &script).await;

    let task = task_with_session(Some("ses_1"), Some("Existing"));
    db::insert_task(&state.db, &task).await.unwrap();

    let agent = state.registry.get("opencode").unwrap();
    backfill_title(
        &state,
        task.id,
        agent,
        "ses_1".to_string(),
        dir.clone(),
        2,
        Duration::from_millis(1),
    )
    .await;

    let stored = db::get_task(&state.db, task.id).await.unwrap().unwrap();
    assert_eq!(stored.session_title.as_deref(), Some("Existing"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A fake `opencode` title lookup that blocks until `release` exists, then
/// prints `json` — lets a test observe the task row mid-`resolve_session_title`.
#[cfg(unix)]
fn blocked_session_list_script(dir: &Path, json: &str, started: &Path, release: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("opencode-blocked-session-list.sh");
    let script = format!(
        "#!/bin/sh\ntouch '{}'\nwhile [ ! -e '{}' ]; do sleep 0.02; done\nprintf '%s' '{json}'\n",
        started.display(),
        release.display(),
    );
    std::fs::write(&path, script).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

/// Regression test for #77: a concurrent `mark_awaiting` must survive a
/// `backfill_title` that captured the row while it was still `Running`.
#[cfg(unix)]
#[tokio::test]
async fn backfill_title_does_not_resurrect_awaiting_input() {
    let dir = std::env::temp_dir().join(format!("favetto-backfill-race-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let started = dir.join("started");
    let release = dir.join("release");
    let script = blocked_session_list_script(
        &dir,
        r#"[{"id":"ses_1","title":"Late title"}]"#,
        &started,
        &release,
    );
    let state = title_state(&dir, &script).await;

    let task = task_with_session(None, None);
    db::insert_task(&state.db, &task).await.unwrap();

    let agent = state.registry.get("opencode").unwrap();
    let handle = tokio::spawn({
        let state = state.clone();
        let agent = agent.clone();
        let dir = dir.clone();
        async move {
            backfill_title(
                &state,
                task.id,
                agent,
                "ses_1".to_string(),
                dir,
                1,
                Duration::from_millis(1),
            )
            .await
        }
    });

    // Wait until the title lookup is actually blocked. At this point the
    // backfill has already captured its `existing` snapshot as `Running`.
    for _ in 0..200 {
        if started.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(started.exists(), "title lookup never started");

    // The watcher marks the task awaiting input while the lookup is in flight.
    assert!(db::set_task_status_if(
        &state.db,
        task.id,
        TaskStatus::AwaitingInput,
        TaskStatus::Running,
    )
    .await
    .unwrap());
    std::fs::write(&release, b"go").unwrap();

    let title = handle.await.unwrap();
    assert_eq!(title.as_deref(), Some("Late title"));

    // The stale `Running` snapshot must not have resurrected the status.
    let stored = db::get_task(&state.db, task.id).await.unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::AwaitingInput);
    assert_eq!(stored.session_id.as_deref(), Some("ses_1"));
    assert_eq!(stored.session_title.as_deref(), Some("Late title"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[tokio::test]
async fn resolve_title_fallback_reads_session_title() {
    let dir = std::env::temp_dir().join(format!("favetto-fallback-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = session_list_script(&dir, r#"[{"id":"ses_1","title":"Fallback title"}]"#);
    let state = title_state(&dir, &script).await;
    let agent = state.registry.get("opencode").unwrap();

    assert_eq!(
        resolve_title_fallback(agent.clone(), Some("ses_1".to_string()), &dir)
            .await
            .as_deref(),
        Some("Fallback title")
    );
    // Without a parsed session id there is nothing to look up.
    assert_eq!(resolve_title_fallback(agent, None, &dir).await, None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[tokio::test]
async fn watch_title_while_running_skips_unknown_session() {
    let dir = std::env::temp_dir().join(format!("favetto-watch-missing-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = session_list_script(&dir, "[]");
    let state = title_state(&dir, &script).await;
    let agent = state.registry.get("opencode").unwrap();

    let watch = tokio::time::timeout(
        Duration::from_secs(2),
        watch_title_while_running(&state, Uuid::new_v4(), agent, "no-such-session", &dir),
    )
    .await
    .expect("watcher returns promptly for an unknown session");
    assert!(matches!(watch, TitleWatch::Skipped));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The acceptance test for #55: the title is resolved and persisted while the
/// run is still in progress (not only at completion).
#[cfg(unix)]
#[tokio::test]
async fn title_is_stored_while_run_is_still_in_progress() {
    let dir = std::env::temp_dir().join(format!("favetto-live-title-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = live_session_script(&dir, r#"[{"id":"ses_live","title":"Live title"}]"#);
    let state = title_state(&dir, &script).await;

    let task = task_with_session(None, None);
    db::insert_task(&state.db, &task).await.unwrap();

    let agent = state.registry.get("opencode").unwrap();
    let ctx = crate::agents::AgentContext {
        cwd: Some(dir.clone()),
        prompt: Some("hi".to_string()),
        rows: 40,
        cols: 120,
        ..Default::default()
    };
    let info = state
        .agents
        .start(
            "opencode",
            agent.clone(),
            Some(task.id.to_string()),
            crate::agents::Invocation::Headless {
                prompt: "hi",
                provider: None,
                model: None,
            },
            ctx,
        )
        .unwrap();

    let watch = tokio::time::timeout(
        Duration::from_secs(10),
        watch_title_while_running(&state, task.id, agent, &info.id, &dir),
    )
    .await
    .expect("title resolves while the run is in progress");
    match watch {
        TitleWatch::Observed(Some(title)) => assert_eq!(title, "Live title"),
        _ => panic!("expected the watcher to observe a title"),
    }

    let stored = db::get_task(&state.db, task.id).await.unwrap().unwrap();
    assert_eq!(stored.session_id.as_deref(), Some("ses_live"));
    assert_eq!(stored.session_title.as_deref(), Some("Live title"));
    // The script is still sleeping: the run has not finished yet.
    assert!(state.agents.is_running(&info.id));

    let _ = state.agents.close(&info.id);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn truncate_text_keeps_head_and_tail_on_char_boundaries() {
    // Multi-byte input: truncation must not split a UTF-8 char.
    let s = "é".repeat(400);
    let (out, truncated) = truncate_text(&s, 100);
    assert!(truncated);
    assert!(out.starts_with('é'), "head preserved");
    assert!(out.ends_with('é'), "tail preserved");
    assert!(out.contains("truncated"), "marker present: {out}");
    // Head + tail stay within the cap; only the marker is added on top.
    assert!(out.len() <= 100 + 60, "capped: {} bytes", out.len());
    // A short input is returned unchanged.
    let (short, truncated) = truncate_text("hello", 100);
    assert_eq!(short, "hello");
    assert!(!truncated);
}

#[test]
fn build_task_output_caps_and_dedupes() {
    let raw = "x".repeat(10_000);
    let parsed = serde_json::json!({ "text": raw });
    let value = build_task_output(&raw, &parsed, "opencode", None, None, 1000);
    assert_eq!(value["result"], serde_json::Value::Null);
    assert_eq!(value["truncated"], true);
    assert_eq!(value["output_bytes"], 10_000);
    assert_eq!(value["agent"], "opencode");
    // The whole stored blob stays in the same order of magnitude as the cap.
    let encoded = serde_json::to_string(&value).unwrap();
    assert!(encoded.len() < 2 * 1000, "bounded blob: {encoded}");
}

#[test]
fn build_task_output_keeps_structured_result() {
    let raw = "done";
    let parsed = serde_json::json!([{ "event": "x" }]);
    let value = build_task_output(raw, &parsed, "opencode", Some("ses_1".into()), None, 1000);
    assert_eq!(value["truncated"], false);
    assert_eq!(value["output"], "done");
    assert_eq!(value["result"], parsed);
    assert_eq!(value["session_id"], "ses_1");
}

#[test]
fn bounded_prev_value_truncates_large_shape_and_keeps_small_verbatim() {
    let small = serde_json::json!({ "ok": true });
    assert_eq!(
        bounded_prev_value(Some(&small), PREV_OUTPUT_BYTES),
        small,
        "a payload under the cap must be untouched"
    );
    assert!(bounded_prev_value(None, PREV_OUTPUT_BYTES).is_null());

    let big = "x".repeat(128 * 1024);
    let stored = serde_json::json!({
        "agent": "opencode",
        "output": big,
        "result": { "preview": big, "truncated": true },
        "truncated": true,
    });
    let bounded = bounded_prev_value(Some(&stored), PREV_OUTPUT_BYTES);
    // The shape survives so `prev.output.<key>` still works, but the bulky
    // duplicate is gone and the raw transcript is capped.
    assert_eq!(bounded["agent"], "opencode");
    assert_eq!(bounded["truncated"], true);
    assert!(bounded["result"].is_null());
    assert!(bounded["output"].as_str().unwrap().len() <= PREV_OUTPUT_BYTES + 64);
    assert!(bounded.to_string().len() < PREV_OUTPUT_BYTES + 512);

    // An input-like object with no known heavy field falls back to a bounded
    // JSON string rather than growing without limit.
    let nested = serde_json::json!({ "body": "z".repeat(200 * 1024) });
    let bounded = bounded_prev_value(Some(&nested), PREV_OUTPUT_BYTES);
    let text = bounded
        .as_str()
        .expect("oversized unknown shape becomes text");
    assert!(text.len() <= PREV_OUTPUT_BYTES + 64);
    assert!(text.contains("truncated"));
}

#[test]
fn branch_name_slugifies_folder_qualified_task() {
    let task = Task {
        id: Uuid::new_v4(),
        name: "pipelines/plan".to_string(),
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
    };
    let short = &task.id.to_string()[..8];
    assert_eq!(
        branch_name(&task),
        format!("favetto/pipelines-plan-{short}")
    );
}

fn wt_record(task_id: Uuid, created_at: chrono::DateTime<Utc>) -> db::WorktreeRecord {
    db::WorktreeRecord {
        task_id,
        repo: PathBuf::from("/repo"),
        path: PathBuf::from(format!("/data/worktrees/{task_id}")),
        branch: format!("favetto/t-{}", &task_id.to_string()[..8]),
        created_at,
    }
}

fn selected_ids(selected: &[db::WorktreeRecord]) -> HashSet<Uuid> {
    selected.iter().map(|r| r.task_id).collect()
}

#[test]
fn terminal_status_classification() {
    assert!(is_terminal(TaskStatus::Succeeded));
    assert!(is_terminal(TaskStatus::Failed));
    assert!(!is_terminal(TaskStatus::Pending));
    assert!(!is_terminal(TaskStatus::Running));
    assert!(!is_terminal(TaskStatus::AwaitingInput));
}

#[test]
fn select_prunable_keeps_active_and_fresh() {
    let now = Utc::now();
    let cutoff = Some(now - chrono::Duration::days(30));
    let fresh = wt_record(Uuid::new_v4(), now);
    let active = wt_record(Uuid::new_v4(), now - chrono::Duration::days(60));

    let mut tasks = HashMap::new();
    tasks.insert(fresh.task_id, (TaskStatus::Succeeded, Some(now)));
    tasks.insert(active.task_id, (TaskStatus::Running, None));

    let selected = select_prunable(&[fresh.clone(), active.clone()], &tasks, cutoff, true, 0);
    assert!(selected.is_empty(), "got {selected:?}");
}

#[test]
fn select_prunable_removes_expired_terminal() {
    let now = Utc::now();
    let cutoff = Some(now - chrono::Duration::days(30));
    let old = wt_record(Uuid::new_v4(), now - chrono::Duration::days(60));

    let mut tasks = HashMap::new();
    tasks.insert(
        old.task_id,
        (
            TaskStatus::Succeeded,
            Some(now - chrono::Duration::days(60)),
        ),
    );

    let selected = select_prunable(std::slice::from_ref(&old), &tasks, cutoff, true, 0);
    assert_eq!(selected_ids(&selected), HashSet::from([old.task_id]));
}

#[test]
fn select_prunable_honors_min_worktrees() {
    let now = Utc::now();
    let cutoff = Some(now - chrono::Duration::days(30));
    // Three expired, terminal worktrees; the newest one is kept.
    let newest = wt_record(Uuid::new_v4(), now - chrono::Duration::days(40));
    let middle = wt_record(Uuid::new_v4(), now - chrono::Duration::days(50));
    let oldest = wt_record(Uuid::new_v4(), now - chrono::Duration::days(60));

    let mut tasks = HashMap::new();
    for (record, finished) in [(&newest, 40), (&middle, 50), (&oldest, 60)] {
        tasks.insert(
            record.task_id,
            (
                TaskStatus::Failed,
                Some(now - chrono::Duration::days(finished)),
            ),
        );
    }

    let selected = select_prunable(
        &[newest.clone(), middle.clone(), oldest.clone()],
        &tasks,
        cutoff,
        true,
        1,
    );
    assert_eq!(
        selected_ids(&selected),
        HashSet::from([middle.task_id, oldest.task_id])
    );
    assert!(!selected_ids(&selected).contains(&newest.task_id));
}

#[test]
fn select_prunable_removes_orphans_and_non_kept() {
    let now = Utc::now();
    let cutoff = Some(now - chrono::Duration::days(30));
    let orphan = wt_record(Uuid::new_v4(), now);
    let not_kept = wt_record(Uuid::new_v4(), now);

    let mut tasks = HashMap::new();
    // The orphan has no task row; the other finished just now.
    tasks.insert(not_kept.task_id, (TaskStatus::Succeeded, Some(now)));

    let selected = select_prunable(
        &[orphan.clone(), not_kept.clone()],
        &tasks,
        cutoff,
        false,
        0,
    );
    assert_eq!(
        selected_ids(&selected),
        HashSet::from([orphan.task_id, not_kept.task_id])
    );
}

#[test]
fn select_prunable_days_zero_keeps_kept_worktrees() {
    let now = Utc::now();
    let kept = wt_record(Uuid::new_v4(), now - chrono::Duration::days(365));
    let orphan = wt_record(Uuid::new_v4(), now);

    let mut tasks = HashMap::new();
    tasks.insert(kept.task_id, (TaskStatus::Succeeded, None));

    // `cutoff == None` disables age-based removal, but orphans still go.
    let selected = select_prunable(&[kept.clone(), orphan.clone()], &tasks, None, true, 0);
    assert_eq!(selected_ids(&selected), HashSet::from([orphan.task_id]));
}

/// End-to-end: a catalog task declaring `[[vars]]` has the collected `input`
/// substituted into its prompt before the agent is launched. Exercises the
/// same `render_context` + `template::render` path as `run_one`, with a fake
/// agent that records the argument vector it received.
#[cfg(unix)]
#[tokio::test]
async fn run_one_renders_input_vars_into_the_agent_prompt() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("favetto-render-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let capture = dir.join("captured-args.txt");
    let script = dir.join("fake-opencode.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\n",
            capture.display()
        ),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();

    let mut cfg = FavettoConfig::default();
    cfg.agent.default = Some("opencode".to_string());
    cfg.agents.insert(
        "opencode".to_string(),
        crate::config::AgentConfig {
            command: script.to_string_lossy().into_owned(),
            ..Default::default()
        },
    );
    let registry = AgentRegistry::from_config_with(&cfg, &|_| true).unwrap();

    let pool = crate::db::open(&dir.join("test.db")).await.unwrap();
    crate::db::migrate(&pool).await.unwrap();
    let state = Arc::new(State::new(StateInit {
        db: pool,
        bus: EventBus::new(64),
        token: Token::generate(),
        webhooks: WebhookSecrets {
            github: None,
            linear: None,
        },
        agents: AgentManager::new(),
        registry,
        config: Arc::new(cfg),
        data_dir: dir.clone(),
        tasks_dir: dir.clone(),
        catalog: Arc::new(RwLock::new(Vec::new())),
        scheduler: tokio_cron_scheduler::JobScheduler::new().await.unwrap(),
        hook_store: Arc::new(RwLock::new(Vec::new())),
    }));

    let def = crate::tasks::parse_task_md(
            "favetto/open_github_issue",
            "agent = \"opencode\"\nmodel = \"deepseek-v4-flash\"\n\
             [[vars]]\nname = \"issue_description\"\nprompt = \"Describe\"\nrequired = true\nmultiline = true\n\
             [[vars]]\nname = \"repo\"\nprompt = \"Repo\"\nrequired = true\n\
             ---\nTarget repository: `{{ input.repo }}`.\n\nDescription: {{ input.issue_description }}\n",
        )
        .unwrap();

    let task = Task {
        id: Uuid::new_v4(),
        name: def.name.clone(),
        status: TaskStatus::Running,
        input: serde_json::json!({
            "repo": "acme/widgets",
            "issue_description": "the widget is broken",
        }),
        output: None,
        dedupe_key: None,
        created_at: Utc::now(),
        started_at: Some(Utc::now()),
        finished_at: None,
        error: None,
        session_id: None,
        session_title: None,
        parent_id: None,
        root_id: None,
    };
    db::insert_task(&state.db, &task).await.unwrap();

    let plan = Plan {
        cwd: dir.clone(),
        needs_lock: false,
        worktree: None,
    };
    run_one(&state, task, def, plan, None).await;

    let captured = std::fs::read_to_string(&capture).unwrap_or_default();
    assert!(
        captured.contains("acme/widgets"),
        "input.repo was not rendered into the prompt: {captured}"
    );
    assert!(
        captured.contains("the widget is broken"),
        "input.issue_description was not rendered into the prompt: {captured}"
    );
    assert!(
        !captured.contains("{{ input."),
        "a raw placeholder survived into the agent prompt: {captured}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A headless run binds a deterministic session id (a UUID) into its args
/// and persists it on the task row, so the finished run can be reopened.
#[cfg(unix)]
#[tokio::test]
async fn run_one_binds_and_persists_a_deterministic_session_id() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("favetto-session-id-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let capture = dir.join("captured-args.txt");
    let script = dir.join("fake-seedy.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\n",
            capture.display()
        ),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();

    let mut cfg = FavettoConfig::default();
    cfg.agent.default = Some("seedy".to_string());
    cfg.agents.insert(
        "seedy".to_string(),
        crate::config::AgentConfig {
            command: script.to_string_lossy().into_owned(),
            headless_args: Some(vec![
                "run".to_string(),
                "--session-id".to_string(),
                "{session_id}".to_string(),
                "{prompt}".to_string(),
            ]),
            resume_args: Some(vec!["resume".to_string(), "{session_id}".to_string()]),
            ..Default::default()
        },
    );
    let registry = AgentRegistry::from_config_with(&cfg, &|_| true).unwrap();

    let pool = crate::db::open(&dir.join("test.db")).await.unwrap();
    crate::db::migrate(&pool).await.unwrap();
    let state = Arc::new(State::new(StateInit {
        db: pool,
        bus: EventBus::new(64),
        token: Token::generate(),
        webhooks: WebhookSecrets {
            github: None,
            linear: None,
        },
        agents: AgentManager::new(),
        registry,
        config: Arc::new(cfg),
        data_dir: dir.clone(),
        tasks_dir: dir.clone(),
        catalog: Arc::new(RwLock::new(Vec::new())),
        scheduler: tokio_cron_scheduler::JobScheduler::new().await.unwrap(),
        hook_store: Arc::new(RwLock::new(Vec::new())),
    }));

    let def = crate::tasks::parse_task_md("t", "agent = \"seedy\"\n---\nbody\n").unwrap();
    let task = Task {
        id: Uuid::new_v4(),
        name: def.name.clone(),
        status: TaskStatus::Running,
        input: serde_json::json!({}),
        output: None,
        dedupe_key: None,
        created_at: Utc::now(),
        started_at: Some(Utc::now()),
        finished_at: None,
        error: None,
        session_id: None,
        session_title: None,
        parent_id: None,
        root_id: None,
    };
    db::insert_task(&state.db, &task).await.unwrap();
    let task_id = task.id;

    let plan = Plan {
        cwd: dir.clone(),
        needs_lock: false,
        worktree: None,
    };
    run_one(&state, task, def, plan, None).await;

    let captured = std::fs::read_to_string(&capture).unwrap_or_default();
    let sid = captured
        .split_whitespace()
        .skip_while(|t| *t != "--session-id")
        .nth(1)
        .expect("--session-id was not passed to the run")
        .to_string();
    assert!(Uuid::parse_str(&sid).is_ok(), "not a UUID: {sid:?}");

    let stored = db::get_task(&state.db, task_id).await.unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::Succeeded);
    assert_eq!(stored.session_id.as_deref(), Some(sid.as_str()));

    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression: an isolated run reads its `spawn_file` from the worktree
/// *before* reclaiming it. The worktree used to be removed before the
/// handoff was read, so the manifest was already gone and the `spawn` tasks
/// were silently dropped.
#[cfg(unix)]
#[tokio::test]
async fn run_one_reads_spawn_file_before_reclaiming_worktree() {
    use std::os::unix::fs::PermissionsExt;

    let root = std::env::temp_dir().join(format!("favetto-spawn-wt-{}", Uuid::new_v4()));
    let repo = root.join("repo");
    let worktree = root.join("worktree");
    std::fs::create_dir_all(&repo).unwrap();

    let run_git = |dir: PathBuf, args: Vec<String>| async move {
        let out = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(&args)
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    };

    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "t@example.com"],
        vec!["config", "user.name", "t"],
    ] {
        run_git(repo.clone(), args.into_iter().map(String::from).collect()).await;
    }
    std::fs::write(repo.join("README.md"), "seed").unwrap();
    for args in [vec!["add", "-A"], vec!["commit", "-qm", "init"]] {
        run_git(repo.clone(), args.into_iter().map(String::from).collect()).await;
    }

    let branch = "favetto/spawn-test";
    run_git(
        repo.clone(),
        vec![
            "worktree".into(),
            "add".into(),
            "--force".into(),
            "-B".into(),
            branch.into(),
            worktree.to_string_lossy().into_owned(),
        ],
    )
    .await;

    // The agent's handoff lives inside the worktree.
    let handoff = worktree.join(".favetto").join("handoff");
    std::fs::create_dir_all(&handoff).unwrap();
    std::fs::write(handoff.join("manifest.json"), r#"[{"issue_id":7}]"#).unwrap();

    // A fake agent that succeeds without doing anything.
    let script = root.join("fake-agent.sh");
    std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();

    let mut cfg = FavettoConfig::default();
    cfg.agent.default = Some("opencode".to_string());
    cfg.agents.insert(
        "opencode".to_string(),
        crate::config::AgentConfig {
            command: script.to_string_lossy().into_owned(),
            ..Default::default()
        },
    );
    let registry = AgentRegistry::from_config_with(&cfg, &|_| true).unwrap();

    let pool = crate::db::open(&root.join("test.db")).await.unwrap();
    crate::db::migrate(&pool).await.unwrap();
    let state = Arc::new(State::new(StateInit {
        db: pool,
        bus: EventBus::new(64),
        token: Token::generate(),
        webhooks: WebhookSecrets {
            github: None,
            linear: None,
        },
        agents: AgentManager::new(),
        registry,
        config: Arc::new(cfg),
        data_dir: root.clone(),
        tasks_dir: root.clone(),
        catalog: Arc::new(RwLock::new(Vec::new())),
        scheduler: tokio_cron_scheduler::JobScheduler::new().await.unwrap(),
        hook_store: Arc::new(RwLock::new(Vec::new())),
    }));

    let def = crate::tasks::parse_task_md(
        "favetto/triage_issues",
        "agent = \"opencode\"\nspawn = \"favetto/plan_issue\"\n\
             spawn_file = \".favetto/handoff/manifest.json\"\n---\nTriage the issues.\n",
    )
    .unwrap();

    let task = Task {
        id: Uuid::new_v4(),
        name: def.name.clone(),
        status: TaskStatus::Running,
        input: serde_json::json!({}),
        output: None,
        dedupe_key: None,
        created_at: Utc::now(),
        started_at: Some(Utc::now()),
        finished_at: None,
        error: None,
        session_id: None,
        session_title: None,
        parent_id: None,
        root_id: None,
    };
    db::insert_task(&state.db, &task).await.unwrap();

    let plan = Plan {
        cwd: worktree.clone(),
        needs_lock: false,
        worktree: Some(Worktree {
            repo: repo.clone(),
            path: worktree.clone(),
            branch: branch.to_string(),
        }),
    };
    let spawner_id = task.id;
    run_one(&state, task, def, plan, None).await;

    let pending = db::next_pending_tasks(&state.db, 10).await.unwrap();
    let spawned = pending
        .iter()
        .find(|t| {
            t.name == "favetto/plan_issue"
                && t.input.get("issue_id").and_then(|v| v.as_i64()) == Some(7)
        })
        .unwrap_or_else(|| {
            panic!(
                "spawn handoff was not enqueued; pending = {:?}",
                pending
                    .iter()
                    .map(|t| (t.name.clone(), t.input.clone()))
                    .collect::<Vec<_>>()
            )
        });
    // The child carries workflow lineage: its parent is the spawner and the
    // spawner (started directly) is its own root.
    assert_eq!(spawned.parent_id, Some(spawner_id));
    assert_eq!(spawned.root_id, Some(spawner_id));
    assert!(
        !worktree.exists(),
        "the worktree should be reclaimed after the handoff is read"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A `State` whose live catalog is `catalog`, built from an explicit config,
/// with no usable agent.
async fn join_state_with_config(
    dir: &Path,
    catalog: Vec<TaskDef>,
    cfg: FavettoConfig,
) -> Arc<State> {
    let pool = crate::db::open(&dir.join("test.db")).await.unwrap();
    crate::db::migrate(&pool).await.unwrap();
    let registry = AgentRegistry::from_config_with(&cfg, &|_| true).unwrap();
    Arc::new(State::new(StateInit {
        db: pool,
        bus: EventBus::new(64),
        token: Token::generate(),
        webhooks: WebhookSecrets {
            github: None,
            linear: None,
        },
        agents: AgentManager::new(),
        registry,
        config: Arc::new(cfg),
        data_dir: dir.to_path_buf(),
        tasks_dir: dir.to_path_buf(),
        catalog: Arc::new(RwLock::new(catalog)),
        scheduler: tokio_cron_scheduler::JobScheduler::new().await.unwrap(),
        hook_store: Arc::new(RwLock::new(Vec::new())),
    }))
}

/// A `State` whose live catalog is `catalog`, with no usable agent.
async fn join_state(dir: &Path, catalog: Vec<TaskDef>) -> Arc<State> {
    join_state_with_config(dir, catalog, FavettoConfig::default()).await
}

/// Regression for issue #91: resuming an agent session must run in the run's
/// worktree, and a worktree reclaimed by retention is recreated so a
/// per-project session store (pi) can find the session again.
#[cfg(unix)]
#[tokio::test]
async fn resume_cwd_recreates_a_reclaimed_worktree() {
    let dir = std::env::temp_dir().join(format!("favetto-resume-cwd-{}", Uuid::new_v4()));
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();

    let run_git = |args: Vec<String>| {
        let repo = repo.clone();
        async move {
            let out = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(&args)
                .output()
                .await
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    };
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "t@example.com"],
        vec!["config", "user.name", "t"],
    ] {
        run_git(args.into_iter().map(String::from).collect()).await;
    }
    std::fs::write(repo.join("README.md"), "seed").unwrap();
    for args in [vec!["add", "-A"], vec!["commit", "-qm", "init"]] {
        run_git(args.into_iter().map(String::from).collect()).await;
    }

    let def = crate::tasks::parse_task_md(
        "issue",
        &format!(
            "agent = \"x\"\ncwd = {:?}\n---\nbody\n",
            repo.to_string_lossy()
        ),
    )
    .unwrap();
    let mut cfg = FavettoConfig::default();
    cfg.executor.parallel = true;
    let state = join_state_with_config(&dir, vec![def], cfg).await;

    let mut task = lineage_task("issue", TaskStatus::Succeeded, None, None);
    task.session_id = Some("ses_1".to_string());
    db::insert_task(&state.db, &task).await.unwrap();

    let expected = dir.join("worktrees").join(task.id.to_string());
    let cwd = resume_cwd(&state, task.id).await.expect("resume cwd");
    assert_eq!(cwd, expected);
    assert!(cwd.exists(), "the worktree was not created");

    // Simulate retention reclaiming the worktree (removes the directory,
    // the branch, and the tracking row).
    remove_worktree(&state.db, task.id, &repo, &cwd, &branch_name(&task)).await;
    assert!(!cwd.exists(), "remove_worktree must delete the directory");

    let recreated = resume_cwd(&state, task.id).await.expect("resume cwd");
    assert_eq!(recreated, expected);
    assert!(
        recreated.exists(),
        "the reclaimed worktree was not recreated"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A catalog task with the given `needs` value.
fn needs_def(name: &str, needs: &str) -> TaskDef {
    crate::tasks::parse_task_md(
        name,
        &format!("agent = \"x\"\nneeds = {needs:?}\n---\nbody\n"),
    )
    .unwrap()
}

/// A task row with explicit lineage. `root = None` makes it its own root.
fn lineage_task(name: &str, status: TaskStatus, root: Option<Uuid>, parent: Option<Uuid>) -> Task {
    Task {
        id: Uuid::new_v4(),
        name: name.to_string(),
        status,
        input: serde_json::json!({}),
        output: None,
        dedupe_key: None,
        created_at: Utc::now(),
        started_at: None,
        finished_at: None,
        error: None,
        session_id: None,
        session_title: None,
        parent_id: parent,
        root_id: root,
    }
}

fn finished_event(name: &str, id: Uuid) -> Event {
    Event {
        id: 1,
        kind: EventKind::TaskFinished,
        payload: serde_json::json!({
            "name": name,
            "task_id": id.to_string(),
            "success": true,
        }),
        created_at: Utc::now(),
    }
}

async fn pending_named(state: &State, name: &str) -> Vec<Task> {
    db::next_pending_tasks(&state.db, 50)
        .await
        .unwrap()
        .into_iter()
        .filter(|t| t.name == name)
        .collect()
}

#[tokio::test]
async fn spawn_new_root_enqueues_children_as_their_own_roots() {
    let dir = std::env::temp_dir().join(format!("favetto-new-root-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = join_state(&dir, Vec::new()).await;
    std::fs::write(
        dir.join("manifest.json"),
        r#"[{"issue_id":1},{"issue_id":2}]"#,
    )
    .unwrap();

    let def = crate::tasks::parse_task_md(
        "favetto/triage_issues",
        "agent = \"x\"\nspawn = \"favetto/plan_issue\"\n\
             spawn_file = \"manifest.json\"\nspawn_new_root = true\n---\nbody\n",
    )
    .unwrap();
    // The spawner itself belongs to a parent workflow root.
    let task = lineage_task(
        "favetto/merge_implementations",
        TaskStatus::Running,
        Some(Uuid::new_v4()),
        Some(Uuid::new_v4()),
    );
    db::insert_task(&state.db, &task).await.unwrap();

    spawn_from_manifest(&state, &task, &def, &dir)
        .await
        .unwrap();

    let children = pending_named(&state, "favetto/plan_issue").await;
    assert_eq!(children.len(), 2);
    for child in &children {
        // A new-root child drops the spawner's lineage: no parent, and it is
        // its own root, so its own fan-in is not deduped against the old root.
        assert_eq!(child.parent_id, None);
        assert_eq!(child.root_id, None);
        assert_eq!(child.root_or_self(), child.id);
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn finished_dependency_fires_per_instance_with_lineage() {
    let dir = std::env::temp_dir().join(format!("favetto-needs-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = join_state(&dir, vec![needs_def("follower", "target:finished")]).await;

    let mut target = lineage_task("target", TaskStatus::Succeeded, None, None);
    target.output = Some(serde_json::json!({ "ok": true }));
    db::insert_task(&state.db, &target).await.unwrap();

    start_dependents(&state, &finished_event("target", target.id)).await;

    let followers = pending_named(&state, "follower").await;
    assert_eq!(followers.len(), 1);
    assert_eq!(
        followers[0].input["_prev"]["task_id"],
        target.id.to_string()
    );
    assert_eq!(followers[0].input["_prev"]["output"]["ok"], true);
    // A dependency is a child of its predecessor; the predecessor is its own
    // root here, so the root is inherited.
    assert_eq!(followers[0].parent_id, Some(target.id));
    assert_eq!(followers[0].root_id, Some(target.id));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn join_waits_for_all_siblings_then_fires_once() {
    let dir = std::env::temp_dir().join(format!("favetto-join-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = join_state(&dir, vec![needs_def("join", "target:all_finished")]).await;

    let root = lineage_task("root", TaskStatus::Succeeded, None, None);
    db::insert_task(&state.db, &root).await.unwrap();
    let a = lineage_task(
        "target",
        TaskStatus::Succeeded,
        Some(root.id),
        Some(root.id),
    );
    let b = lineage_task("target", TaskStatus::Running, Some(root.id), Some(root.id));
    db::insert_task(&state.db, &a).await.unwrap();
    db::insert_task(&state.db, &b).await.unwrap();

    start_dependents(&state, &finished_event("target", a.id)).await;
    assert!(
        pending_named(&state, "join").await.is_empty(),
        "the join fired while a sibling was still running"
    );

    let mut b_done = b.clone();
    b_done.status = TaskStatus::Succeeded;
    db::upsert_task(&state.db, &b_done).await.unwrap();
    start_dependents(&state, &finished_event("target", b.id)).await;

    let joins = pending_named(&state, "join").await;
    assert_eq!(joins.len(), 1);
    let prev = &joins[0].input["_prev"];
    assert_eq!(prev["kind"], "all_finished");
    assert_eq!(prev["target"], "target");
    assert_eq!(prev["count"], 2);
    assert_eq!(prev["succeeded"], 2);
    assert_eq!(prev["failed"], 0);
    assert_eq!(prev["root"]["task_id"], root.id.to_string());
    assert_eq!(prev["root"]["name"], "root");
    assert_eq!(prev["tasks"].as_array().unwrap().len(), 2);
    // The join inherits the root and records the last finisher as parent.
    assert_eq!(joins[0].parent_id, Some(b.id));
    assert_eq!(joins[0].root_id, Some(root.id));

    // Replaying the same finish event cannot enqueue the join twice.
    start_dependents(&state, &finished_event("target", b.id)).await;
    assert_eq!(pending_named(&state, "join").await.len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn finished_dependency_bounds_large_output() {
    let dir = std::env::temp_dir().join(format!("favetto-needs-budget-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = join_state(&dir, vec![needs_def("follower", "target:finished")]).await;

    let mut target = lineage_task("target", TaskStatus::Succeeded, None, None);
    let big = "y".repeat(200 * 1024);
    target.output = Some(serde_json::json!({
        "agent": "opencode",
        "output": big,
        "result": { "preview": big, "truncated": true },
    }));
    db::insert_task(&state.db, &target).await.unwrap();

    start_dependents(&state, &finished_event("target", target.id)).await;
    let followers = pending_named(&state, "follower").await;
    assert_eq!(followers.len(), 1);
    let prev = &followers[0].input["_prev"];
    assert!(prev["output"]["output"].as_str().unwrap().len() <= PREV_OUTPUT_BYTES + 64);
    assert!(
        prev.to_string().len() < PREV_OUTPUT_BYTES + 1024,
        "single-dep _prev is {} bytes",
        prev.to_string().len()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn join_aggregate_bounds_large_outputs() {
    let dir = std::env::temp_dir().join(format!("favetto-join-budget-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = join_state(&dir, vec![needs_def("join", "target:all_finished")]).await;
    let root = lineage_task("root", TaskStatus::Succeeded, None, None);
    db::insert_task(&state.db, &root).await.unwrap();

    // Five successes, each with a full-size (~256 KiB) transcript plus the
    // parsed duplicate: together they used to render a multi-megabyte prompt.
    // One also carries a huge nested input, which must be bounded too.
    let big = "x".repeat(256 * 1024);
    for i in 0..5 {
        let mut t = lineage_task(
            "target",
            TaskStatus::Succeeded,
            Some(root.id),
            Some(root.id),
        );
        if i == 0 {
            t.input = serde_json::json!({ "issue_id": 1, "body": "z".repeat(200 * 1024) });
        }
        t.output = Some(serde_json::json!({
            "agent": "opencode",
            "output": big,
            "result": { "preview": big, "truncated": true },
            "truncated": true,
        }));
        db::insert_task(&state.db, &t).await.unwrap();
    }

    evaluate_join_barriers(&state, "target", root.id, Some(root.id)).await;
    let joins = pending_named(&state, "join").await;
    assert_eq!(joins.len(), 1);
    let prev = &joins[0].input["_prev"];
    assert_eq!(prev["tasks"].as_array().unwrap().len(), 5);
    let rendered = prev.to_string();
    assert!(
        rendered.len() < PREV_TASKS_BYTES + 16 * 1024,
        "fan-in aggregate is {} bytes, over the {} byte budget",
        rendered.len(),
        PREV_TASKS_BYTES
    );
    // Metadata is kept; the bulky duplicate, raw transcript, and nested
    // input are all bounded.
    let first = &prev["tasks"][0];
    assert_eq!(first["success"], true);
    assert_eq!(first["name"], "target");
    assert_eq!(first["output"]["truncated"], true);
    assert!(first["output"]["result"].is_null());
    assert!(first["input"].to_string().len() <= PREV_OUTPUT_BYTES + 1024);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn joins_are_root_scoped() {
    let dir = std::env::temp_dir().join(format!("favetto-join-roots-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = join_state(&dir, vec![needs_def("join", "target:all_finished")]).await;

    let root1 = lineage_task("root1", TaskStatus::Succeeded, None, None);
    let root2 = lineage_task("root2", TaskStatus::Succeeded, None, None);
    db::insert_task(&state.db, &root1).await.unwrap();
    db::insert_task(&state.db, &root2).await.unwrap();
    let a1 = lineage_task(
        "target",
        TaskStatus::Succeeded,
        Some(root1.id),
        Some(root1.id),
    );
    let b2 = lineage_task(
        "target",
        TaskStatus::Running,
        Some(root2.id),
        Some(root2.id),
    );
    db::insert_task(&state.db, &a1).await.unwrap();
    db::insert_task(&state.db, &b2).await.unwrap();

    // Root 1's only target finished: its join starts, root 2 is untouched.
    start_dependents(&state, &finished_event("target", a1.id)).await;
    let joins = pending_named(&state, "join").await;
    assert_eq!(joins.len(), 1);
    assert_eq!(joins[0].root_id, Some(root1.id));

    // A stale finish event while root 2's target is still running must not
    // resolve root 2's barrier (the check is derived from the database).
    start_dependents(&state, &finished_event("target", b2.id)).await;
    assert_eq!(pending_named(&state, "join").await.len(), 1);

    let mut b2_done = b2.clone();
    b2_done.status = TaskStatus::Succeeded;
    db::upsert_task(&state.db, &b2_done).await.unwrap();
    start_dependents(&state, &finished_event("target", b2.id)).await;
    let joins = pending_named(&state, "join").await;
    assert_eq!(joins.len(), 2);
    let roots: Vec<Uuid> = joins.iter().filter_map(|t| t.root_id).collect();
    assert!(roots.contains(&root1.id));
    assert!(roots.contains(&root2.id));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn empty_fan_out_resolves_barrier() {
    let dir = std::env::temp_dir().join(format!("favetto-join-empty-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = join_state(&dir, vec![needs_def("join", "target:all_finished")]).await;
    let root = lineage_task("root", TaskStatus::Succeeded, None, None);
    db::insert_task(&state.db, &root).await.unwrap();

    // No `target` run was ever spawned: the empty barrier still resolves.
    evaluate_join_barriers(&state, "target", root.id, Some(root.id)).await;
    let joins = pending_named(&state, "join").await;
    assert_eq!(joins.len(), 1);
    let prev = &joins[0].input["_prev"];
    assert_eq!(prev["count"], 0);
    assert_eq!(prev["tasks"].as_array().unwrap().len(), 0);
    assert_eq!(prev["succeeded"], 0);

    // The dedupe key keeps a re-evaluation from enqueueing another join.
    evaluate_join_barriers(&state, "target", root.id, Some(root.id)).await;
    assert_eq!(pending_named(&state, "join").await.len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn failed_and_cancelled_children_satisfy_barrier() {
    let dir = std::env::temp_dir().join(format!("favetto-join-fail-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = join_state(&dir, vec![needs_def("join", "target:all_finished")]).await;
    let root = lineage_task("root", TaskStatus::Succeeded, None, None);
    db::insert_task(&state.db, &root).await.unwrap();
    let failed = lineage_task("target", TaskStatus::Failed, Some(root.id), Some(root.id));
    let cancelled = lineage_task(
        "target",
        TaskStatus::Cancelled,
        Some(root.id),
        Some(root.id),
    );
    db::insert_task(&state.db, &failed).await.unwrap();
    db::insert_task(&state.db, &cancelled).await.unwrap();

    start_dependents(&state, &finished_event("target", failed.id)).await;
    let joins = pending_named(&state, "join").await;
    assert_eq!(joins.len(), 1);
    let prev = &joins[0].input["_prev"];
    assert_eq!(prev["count"], 2);
    assert_eq!(prev["succeeded"], 0);
    assert_eq!(prev["failed"], 1);
    assert_eq!(prev["cancelled"], 1);

    let _ = std::fs::remove_dir_all(&dir);
}
