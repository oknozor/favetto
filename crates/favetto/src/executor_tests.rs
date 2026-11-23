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
        kind: TaskKind::Agent,
        action: None,
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
        permission: None,
        worktree: None,
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
        attempt: 0,
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
        failure: None,
        session_id: None,
        session_title: None,
        parent_id: None,
        root_id: None,
        interactive: false,
    };
    let ctx = render_context(&task);
    assert_eq!(crate::template::render("{{ task.name }}", &ctx), "triage");
    assert_eq!(crate::template::render("{{ input.issue_id }}", &ctx), "7");
    assert_eq!(crate::template::render("{{ prev.output }}", &ctx), "done");
}

fn task_with_session(session_id: Option<&str>, session_title: Option<&str>) -> Task {
    Task {
        id: Uuid::new_v4(),
        name: "t".to_string(),
        status: TaskStatus::Running,
        attempt: 0,
        input: serde_json::json!({}),
        output: None,
        dedupe_key: None,
        created_at: Utc::now(),
        started_at: None,
        finished_at: None,
        error: None,
        failure: None,
        session_id: session_id.map(str::to_string),
        session_title: session_title.map(str::to_string),
        parent_id: None,
        root_id: None,
        interactive: false,
    }
}

/// A running attempt for a task inserted directly by a test (the state a task is
/// in after `db::claim_task`). Used to drive `run_one` without the dispatcher.
fn running_run(task_id: Uuid) -> TaskRun {
    TaskRun {
        id: Uuid::new_v4(),
        task_id,
        attempt: 1,
        status: RunStatus::Running,
        agent: None,
        model: None,
        session_id: None,
        started_at: Some(Utc::now()),
        finished_at: None,
        exit_code: None,
        error: None,
        usage: None,
        failure: None,
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
            exit_code: None,
            usage: None,
            failure: Some(Failure::new(FailureKind::Agent, "exit 1")),
            alive: false,
        }),
    );
    assert!(!success);
    assert_eq!(task.status, TaskStatus::Failed);
    assert_eq!(task.session_id.as_deref(), Some("ses_1"));
    assert_eq!(task.session_title.as_deref(), Some("Fix the widget"));
    assert!(task.error.as_deref().unwrap().contains("exit 1"));
    assert_eq!(
        task.failure.as_ref().map(|f| f.kind),
        Some(FailureKind::Agent)
    );
    assert!(!task.failure.as_ref().unwrap().retryable);
    // A failed run keeps its (structured) output so the web can still render
    // the post-run agent log.
    assert_eq!(task.output, Some(serde_json::json!({})));
}

#[test]
fn successful_run_records_output_and_session() {
    let mut task = task_with_session(None, None);
    task.error = Some("stale".to_string());
    task.failure = Some(Failure::new(FailureKind::Agent, "stale"));
    let success = record_run_outcome(
        &mut task,
        Ok(RunOutcome {
            output: serde_json::json!({ "ok": true }),
            session_id: Some("ses_1".to_string()),
            session_title: Some("Fix the widget".to_string()),
            exit_code: None,
            usage: None,
            failure: None,
            alive: false,
        }),
    );
    assert!(success);
    assert_eq!(task.status, TaskStatus::Succeeded);
    assert_eq!(task.output, Some(serde_json::json!({ "ok": true })));
    assert_eq!(task.session_id.as_deref(), Some("ses_1"));
    assert_eq!(task.session_title.as_deref(), Some("Fix the widget"));
    assert!(task.error.is_none());
    // A successful run clears a failure left by an earlier attempt.
    assert!(task.failure.is_none());
}

#[test]
fn reported_usage_prefers_structured_output_then_live_state() {
    let live = AgentUsage {
        input_tokens: 7,
        cost_usd: Some(0.01),
        ..Default::default()
    };

    // Structured `RunSummary.usage` wins over the live fallback.
    let summary = serde_json::to_value(RunSummary {
        usage: AgentUsage {
            input_tokens: 3,
            output_tokens: 2,
            cost_usd: Some(0.5),
            ..Default::default()
        },
        ..Default::default()
    })
    .unwrap();
    let got = reported_usage(&summary, Some(live.clone())).expect("structured usage");
    assert_eq!(got.input_tokens, 3);
    assert_eq!(got.output_tokens, 2);

    // No structured usage: fall back to the folded live state.
    let got = reported_usage(&serde_json::json!({ "text": "hi" }), Some(live)).expect("live usage");
    assert_eq!(got.input_tokens, 7);

    // Nothing reported anywhere: stored as `None`.
    assert_eq!(
        reported_usage(&serde_json::json!({ "text": "hi" }), None),
        None
    );
}

#[test]
fn run_without_agent_still_fails() {
    let mut task = task_with_session(Some("ses_keep"), Some("Keep"));
    let success = record_run_outcome(
        &mut task,
        Err(RunError::new(FailureKind::Infrastructure, "no agent")),
    );
    assert!(!success);
    assert_eq!(task.status, TaskStatus::Failed);
    // A pre-run failure never drops an already-resolved session.
    assert_eq!(task.session_id.as_deref(), Some("ses_keep"));
    assert_eq!(task.session_title.as_deref(), Some("Keep"));
    assert!(task.error.as_deref().unwrap().contains("no agent"));
    assert_eq!(
        task.failure.as_ref().map(|f| f.kind),
        Some(FailureKind::Infrastructure)
    );
    assert!(task.failure.as_ref().unwrap().retryable);
}

/// A run that produced no exit code (the PTY/session vanished) is an
/// infrastructure fault, not an agent failure.
#[test]
fn vanished_session_is_infrastructure() {
    let mut task = task_with_session(None, None);
    let success = record_run_outcome(
        &mut task,
        Ok(RunOutcome {
            output: serde_json::json!({}),
            session_id: None,
            session_title: None,
            exit_code: None,
            usage: None,
            failure: Some(Failure::new(
                FailureKind::Infrastructure,
                "agent 'opencode' session disappeared",
            )),
            alive: false,
        }),
    );
    assert!(!success);
    assert_eq!(
        task.failure.as_ref().map(|f| f.kind),
        Some(FailureKind::Infrastructure)
    );
}

/// The resolved design decision: `Blocked` is a `FailureKind`, terminal and
/// never retried automatically.
#[test]
fn blocked_failure_is_not_retryable() {
    let failure = Failure::new(FailureKind::Blocked, "credentials");
    assert_eq!(failure.kind, FailureKind::Blocked);
    assert!(!failure.retryable);
}

/// The dispatcher's catalog miss names the full target, so a copied reason
/// identifies the spawn even outside the UI.
#[test]
fn catalog_miss_message_names_the_full_target() {
    assert_eq!(
        catalog_miss_message("repo:Favetto/plan_issue"),
        "task 'repo:Favetto/plan_issue' not found in the catalog"
    );
}

/// `fail_task` persists both the human-readable `error` and the typed
/// `failure`, so a controller can classify a task that never started.
#[tokio::test]
async fn fail_task_records_the_requested_kind() {
    let dir = std::env::temp_dir().join(format!("favetto-fail-task-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = join_state(&dir, Vec::new()).await;

    let mut task = task_with_session(None, None);
    // A pre-dispatch failure is still `pending` when `fail_task` runs.
    task.status = TaskStatus::Pending;
    db::insert_task(&state.db, &task).await.unwrap();

    fail_task(
        &state,
        task.clone(),
        FailureKind::InvalidInput,
        "task not found in the catalog",
    )
    .await;

    let stored = db::get_task(&state.db, task.id).await.unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::Failed);
    assert_eq!(
        stored.error.as_deref(),
        Some("task not found in the catalog")
    );
    let failure = stored.failure.expect("failure recorded");
    assert_eq!(failure.kind, FailureKind::InvalidInput);
    assert_eq!(failure.message, "task not found in the catalog");
    assert!(!failure.retryable);
    assert_eq!(stored.attempt, 1, "the failed attempt is counted");

    // The failure is recorded as exactly one failed run.
    let runs = db::list_task_runs(&state.db, task.id).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, RunStatus::Failed);
    assert_eq!(runs[0].attempt, 1);
    assert_eq!(
        runs[0].failure.as_ref().map(|f| f.kind),
        Some(FailureKind::InvalidInput)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The additive payload keeps every legacy key and layers the machine-readable
/// finish context on top, so a consumer need not fetch the task.
#[test]
fn finished_payload_keeps_legacy_fields_and_adds_context() {
    let mut task = task_with_session(None, None);
    task.status = TaskStatus::Succeeded;
    task.attempt = 1;
    task.output = Some(serde_json::json!({
        "envelope": { "summary": "Added the widget" }
    }));

    let payload = finished_payload(&task, true);
    assert_eq!(payload["name"], "t");
    assert_eq!(payload["task_id"], task.id.to_string());
    assert_eq!(payload["success"], true);
    assert_eq!(payload["status"], "succeeded");
    assert_eq!(payload["attempt"], 1);
    assert_eq!(payload["retryable"], false);
    assert_eq!(payload["summary"], "Added the widget");
}

#[test]
fn finished_payload_reports_a_retryable_failure() {
    let mut task = task_with_session(None, None);
    task.status = TaskStatus::Failed;
    task.attempt = 1;
    task.error = Some("PTY died".to_string());
    task.failure = Some(Failure::new(FailureKind::Infrastructure, "PTY died"));

    let payload = finished_payload(&task, false);
    assert_eq!(payload["success"], false);
    assert_eq!(payload["status"], "failed");
    assert_eq!(payload["attempt"], 1);
    assert_eq!(payload["retryable"], true);
    // A failed task has no output, so the error is the bounded summary.
    assert_eq!(payload["summary"], "PTY died");
}

#[test]
fn finished_payload_marks_agent_failures_non_retryable() {
    let mut task = task_with_session(None, None);
    task.status = TaskStatus::Failed;
    task.error = Some("exit 1".to_string());
    task.failure = Some(Failure::new(FailureKind::Agent, "exit 1"));

    let payload = finished_payload(&task, false);
    assert_eq!(payload["status"], "failed");
    assert_eq!(payload["retryable"], false);
}

#[test]
fn finished_payload_omits_an_empty_summary() {
    let mut task = task_with_session(None, None);
    task.status = TaskStatus::Succeeded;
    task.output = Some(serde_json::json!({ "envelope": { "summary": "  " } }));

    let payload = finished_payload(&task, true);
    assert!(
        payload.get("summary").is_none(),
        "a blank summary must be omitted, got {:?}",
        payload.get("summary")
    );
}

#[test]
fn finished_payload_bounds_a_long_summary() {
    let mut task = task_with_session(None, None);
    task.status = TaskStatus::Succeeded;
    task.output = Some(serde_json::json!({
        "envelope": { "summary": "x".repeat(4096) }
    }));

    let payload = finished_payload(&task, true);
    let summary = payload["summary"].as_str().expect("summary present");
    assert!(summary.len() < 4096, "summary was not truncated");
    // `tail_truncate` prefixes the `…` ellipsis (3 UTF-8 bytes).
    assert!(
        summary.len() <= FINISHED_SUMMARY_BYTES + 3,
        "summary exceeds the cap: {} bytes",
        summary.len()
    );
}

/// `fail_task` emits the enriched `TaskFinished` event, not just the legacy keys.
#[tokio::test]
async fn fail_task_emits_an_enriched_finished_event() {
    let dir = std::env::temp_dir().join(format!("favetto-finish-fail-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = join_state(&dir, Vec::new()).await;

    let mut task = task_with_session(None, None);
    // A pre-dispatch failure is still `pending` when `fail_task` runs, so the
    // claim succeeds and the failed run is counted as attempt 1.
    task.status = TaskStatus::Pending;
    db::insert_task(&state.db, &task).await.unwrap();

    fail_task(
        &state,
        task.clone(),
        FailureKind::Infrastructure,
        "spawn failed",
    )
    .await;

    let events = db::tail_events(&state.db, 10).await.unwrap();
    let finished = events
        .iter()
        .rev()
        .find(|e| e.kind == EventKind::TaskFinished)
        .expect("TaskFinished emitted");
    assert_eq!(finished.payload["name"], "t");
    assert_eq!(finished.payload["task_id"], task.id.to_string());
    assert_eq!(finished.payload["success"], false);
    assert_eq!(finished.payload["status"], "failed");
    assert_eq!(finished.payload["attempt"], 1);
    assert_eq!(finished.payload["retryable"], true);
    assert_eq!(finished.payload["summary"], "spawn failed");

    let _ = std::fs::remove_dir_all(&dir);
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
            exit_code: None,
            usage: None,
            failure: None,
            alive: false,
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
            exit_code: None,
            usage: None,
            failure: Some(Failure::new(FailureKind::Agent, "exit 1")),
            alive: false,
        }),
    );
    assert!(!success);
    assert_eq!(failed.status, TaskStatus::Failed);
}

/// A fake `opencode session list --format json` executable that ignores its
/// arguments and prints `json`.
#[cfg(unix)]
/// A fake `opencode` that reports a live session id and keeps running unless
/// invoked as `session list`, where it prints `title_json`.
#[cfg(unix)]
/// A `State` whose `opencode` command is `command` (a title-lookup fixture).
#[cfg(unix)]
#[cfg(unix)]
#[cfg(unix)]
/// A fake `opencode` title lookup that blocks until `release` exists, then
/// prints `json` — lets a test observe the task row mid-`resolve_session_title`.
#[cfg(unix)]
/// Regression test for #77: a concurrent `mark_awaiting` must survive a
/// `backfill_title` that captured the row while it was still `Running`.
#[cfg(unix)]
#[cfg(unix)]
#[cfg(unix)]
/// The acceptance test for #55: the title is resolved and persisted while the
/// run is still in progress (not only at completion).
#[cfg(unix)]
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
    let value = build_task_output(&raw, &parsed, Some("opencode"), None, None, 1000);
    assert_eq!(value["result"], serde_json::Value::Null);
    assert_eq!(value["truncated"], true);
    assert_eq!(value["output_bytes"], 10_000);
    assert_eq!(value["agent"], "opencode");
    // The whole stored blob stays in the same order of magnitude as the cap.
    let encoded = serde_json::to_string(&value).unwrap();
    assert!(encoded.len() < 2 * 1000, "bounded blob: {encoded}");
}

/// The persisted `task.output` blob must stay bounded for *every* envelope shape
/// the parser can produce, not just the default `{"text": raw}` one. Pins the
/// `[executor].max_output_bytes` guarantee (issue #207) and round-trips the
/// largest shape through SQLite so the stored blob is bounded too.
#[tokio::test]
async fn build_task_output_bounds_every_shape() {
    let max = 1000usize;
    let raw = "x".repeat(50_000);
    let huge = "y".repeat(200_000);

    let shapes: Vec<serde_json::Value> = vec![
        // Default parser: raw text wrapped as `{"text": raw}`.
        serde_json::json!({ "text": raw }),
        // Structured envelope with oversized artifacts / findings / outputs.
        serde_json::json!({
            "summary": huge,
            "artifacts": [{ "kind": "source", "path": huge }],
            "findings": [{ "note": huge }],
            "outputs": { "blob": huge },
            "continuation": { "spawn": "next" },
        }),
        // Non-envelope result: a huge array with no expected shape.
        serde_json::json!([{ "event": "x" }, { "event": huge }]),
        // Unknown deeply-nested object that is not an envelope.
        serde_json::json!({ "foo": { "bar": { "baz": [huge] } } }),
    ];

    for (i, parsed) in shapes.iter().enumerate() {
        let value = build_task_output(&raw, parsed, Some("agent"), None, None, max);
        assert_eq!(
            value["output_bytes"].as_u64(),
            Some(raw.len() as u64),
            "shape {i}"
        );
        assert_eq!(value["truncated"], true, "shape {i}");
        let encoded = serde_json::to_string(&value).unwrap();
        assert!(
            encoded.len() < 4 * max,
            "shape {i} stored {} bytes (cap {max})",
            encoded.len()
        );
    }

    // Round-trip the largest shape through the DB: the persisted blob stays
    // bounded too.
    let dir = std::env::temp_dir().join(format!("favetto-output-bound-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let pool = crate::db::open(&dir.join("test.db")).await.unwrap();
    crate::db::migrate(&pool).await.unwrap();

    let built = build_task_output(
        &raw,
        &serde_json::json!([{ "event": huge }]),
        Some("agent"),
        None,
        None,
        max,
    );
    let task = Task {
        id: Uuid::new_v4(),
        name: "bounded".to_string(),
        status: TaskStatus::Succeeded,
        attempt: 1,
        input: serde_json::json!({}),
        output: Some(built),
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
    };
    crate::db::upsert_task(&pool, &task).await.unwrap();
    let stored = crate::db::get_task(&pool, task.id)
        .await
        .unwrap()
        .expect("task row");
    let encoded = serde_json::to_string(&stored.output.expect("output persisted")).unwrap();
    assert!(
        encoded.len() < 4 * max,
        "persisted blob is {} bytes (cap {max})",
        encoded.len()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn build_task_output_keeps_structured_result() {
    let raw = "done";
    let parsed = serde_json::json!([{ "event": "x" }]);
    let value = build_task_output(
        raw,
        &parsed,
        Some("opencode"),
        Some("ses_1".into()),
        None,
        1000,
    );
    assert_eq!(value["truncated"], false);
    assert_eq!(value["output"], "done");
    assert_eq!(value["result"], parsed);
    assert_eq!(value["session_id"], "ses_1");
}

#[test]
fn build_task_output_keeps_a_truncated_run_summary_decodable() {
    let big = "x".repeat(20_000);
    let summary = RunSummary {
        session_id: Some("ses_1".to_string()),
        title: Some("Fix it".to_string()),
        text: big.clone(),
        reasoning: big.clone(),
        tool_calls: vec![favetto_core::model::ToolCall {
            id: "call_1".to_string(),
            name: "bash".to_string(),
            input: serde_json::json!({ "command": big }),
            ok: true,
            output: Some(serde_json::json!(big)),
            detail: None,
        }],
        usage: AgentUsage {
            input_tokens: 5,
            ..Default::default()
        },
        outcome: Some(IdleOutcome::Succeeded),
        error: None,
    };
    let parsed = serde_json::to_value(&summary).unwrap();
    let max = 1_000;
    let value = build_task_output(
        &big,
        &parsed,
        Some("acp-agent"),
        Some("ses_1".to_string()),
        Some("Fix it".to_string()),
        max,
    );

    // The summary is bounded field-by-field and stays a decodable run log; a
    // `{ preview, truncated }` stub would force the plain fallback.
    assert!(value["result"].get("tool_calls").is_some());
    let decoded: RunSummary =
        serde_json::from_value(value["result"].clone()).expect("decodable summary");
    assert!(!decoded.is_empty());
    assert!(decoded.text.contains("truncated"));
    assert_eq!(decoded.session_id.as_deref(), Some("ses_1"));
    assert_eq!(decoded.tool_calls.len(), 1);
    assert_eq!(decoded.outcome, Some(IdleOutcome::Succeeded));
    assert_eq!(value["truncated"], true);
    assert_eq!(value["session_title"], "Fix it");
    let encoded = serde_json::to_string(&value).unwrap();
    assert!(encoded.len() < 4 * max, "bounded blob: {}", encoded.len());
}

#[test]
fn build_task_output_leaves_legacy_result_shapes_alone() {
    // An old ad-hoc result (`{text, session_id}`) is not a serialized summary;
    // it keeps the pre-existing pass-through so old rows still render.
    let parsed = serde_json::json!({ "text": "plain answer", "session_id": "ses_1" });
    let value = build_task_output("plain answer", &parsed, Some("pty"), None, None, 1_000);
    assert_eq!(value["result"], parsed);
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
fn build_task_output_synthesises_summary_from_raw_tail() {
    let raw = "starting\nstep one\ndone: added the feature\n";
    let parsed = serde_json::json!({ "text": raw });
    let value = build_task_output(raw, &parsed, Some("plain"), None, None, 1000);
    assert_eq!(value["envelope"]["summary"], "done: added the feature");
    assert_eq!(value["envelope"]["artifacts"], serde_json::json!([]));
    assert_eq!(value["envelope"]["findings"], serde_json::json!([]));
    assert_eq!(value["envelope"]["outputs"], serde_json::json!({}));
    assert_eq!(value["envelope"]["continuation"], serde_json::Value::Null);
    // The default `{ "text": raw }` duplicate is still dropped.
    assert_eq!(value["result"], serde_json::Value::Null);
    assert_eq!(value["agent"], "plain");
    assert_eq!(value["output"], raw);
}

#[test]
fn build_task_output_embeds_structured_envelope() {
    let parsed = serde_json::json!({
        "summary": "did it",
        "artifacts": [{ "kind": "source", "path": "a.rs" }],
        "findings": [{ "note": "n" }],
        "outputs": { "tests_passed": true },
        "continuation": { "spawn": "next" },
    });
    let value = build_task_output("raw text", &parsed, Some("agent"), None, None, 1000);
    assert_eq!(value["envelope"], parsed);
    // The envelope is the canonical copy, so `result` is de-duped.
    assert_eq!(value["result"], serde_json::Value::Null);
    assert_eq!(value["output"], "raw text");
    assert_eq!(value["agent"], "agent");
}

#[test]
fn build_task_output_embeds_envelope_from_result_text() {
    let parsed = serde_json::json!({
        "text": "{\"summary\":\"from text\",\"artifacts\":[]}",
        "session_id": "ses_1",
    });
    let value = build_task_output("whatever", &parsed, Some("opencode"), None, None, 1000);
    assert_eq!(value["envelope"]["summary"], "from text");
    // Missing documented keys are normalized in.
    assert_eq!(value["envelope"]["findings"], serde_json::json!([]));
    assert_eq!(value["envelope"]["outputs"], serde_json::json!({}));
    // The envelope came from `text`, not from the result object itself, so the
    // result is preserved.
    assert_eq!(value["result"], parsed);
}

#[test]
fn build_task_output_ignores_non_envelope_result() {
    let raw = "line a\nfinal answer";
    let parsed = serde_json::json!({
        "text": "this is not JSON",
        "session_id": "ses_1",
    });
    let value = build_task_output(raw, &parsed, Some("opencode"), None, None, 1000);
    assert_eq!(value["envelope"]["summary"], "final answer");
    assert_eq!(value["result"], parsed);
}

#[test]
fn bounded_prev_value_bounds_envelope_arrays() {
    let big = "x".repeat(200 * 1024);
    let stored = serde_json::json!({
        "agent": "opencode",
        "output": "small transcript",
        "envelope": {
            "summary": "s",
            "artifacts": [{ "kind": "source", "path": big }],
            "findings": [{ "note": big }],
            "outputs": { "blob": big },
            "continuation": null,
        },
    });
    let bounded = bounded_prev_value(Some(&stored), PREV_OUTPUT_BYTES);
    assert_eq!(bounded["truncated"], true);
    // Each bulky field is replaced by a bounded string.
    for key in ["artifacts", "findings", "outputs"] {
        let field = &bounded["envelope"][key];
        assert!(field.is_string(), "{key} should be stringified: {field}");
        assert!(field.as_str().unwrap().len() <= PREV_OUTPUT_BYTES / 3 + 128);
    }
    assert!(bounded["envelope"]["summary"].is_string());
    assert!(
        bounded.to_string().len() < 2 * PREV_OUTPUT_BYTES,
        "bounded _prev is {} bytes",
        bounded.to_string().len()
    );
}

#[test]
fn branch_name_slugifies_folder_qualified_task() {
    let task = Task {
        id: Uuid::new_v4(),
        name: "pipelines/plan".to_string(),
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
    assert!(is_terminal(TaskStatus::Cancelled));
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
/// A headless run binds a deterministic session id (a UUID) into its args
/// and persists it on the task row, so the finished run can be reopened.
#[cfg(unix)]
/// A failing run-one finalizes the attempt's run as `failed`, carrying the exit
/// code and typed failure, while `tasks.get` still returns the task.
#[cfg(unix)]
/// Regression: `tasks.cancel` marks a running task `cancelled` and terminates
/// its agent. When the run's coroutine then unwinds, its own outcome must not
/// overwrite the cancelled row nor emit `TaskFinished` (which would schedule
/// `needs`/join successors).
#[cfg(unix)]
/// A retryable failure re-enqueues the task for a fresh attempt instead of
/// finalizing it. The failed attempt still records exactly one run, and no
/// `TaskFinished` fires until the attempt budget is spent.
/// A genuine agent failure is not in the default `retry_on`, so it terminates
/// immediately even with a retry budget configured.
#[cfg(unix)]
/// Once the attempt budget is spent, even a retryable failure terminates.
#[tokio::test]
async fn run_one_stops_retrying_once_the_attempt_budget_is_spent() {
    let dir = std::env::temp_dir().join(format!("favetto-retry-budget-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();

    let mut cfg = FavettoConfig::default();
    cfg.agent.default = Some("ghost".to_string());
    cfg.executor.retry.max_attempts = 2;
    cfg.executor.retry.retry_on = vec![FailureKind::Infrastructure];
    let def = crate::tasks::parse_task_md("t", "---\nbody\n").unwrap();
    let state = join_state_with_config(&dir, vec![def.clone()], cfg).await;

    let task = task_with_session(None, None);
    db::insert_task(&state.db, &task).await.unwrap();

    let plan = Plan {
        cwd: dir.clone(),
        needs_lock: false,
        worktree: None,
    };
    // The second attempt is already the last one.
    let mut run = running_run(task.id);
    run.attempt = 2;
    db::insert_task_run(&state.db, &run).await.unwrap();
    run_one(&state, task.clone(), run, def, plan, None).await;

    let stored = db::get_task(&state.db, task.id).await.unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::Failed);
    assert_eq!(stored.attempt, 2);
    let events = db::tail_events(&state.db, 20).await.unwrap();
    assert!(
        events.iter().any(|e| e.kind == EventKind::TaskFinished),
        "the final attempt emits TaskFinished"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `run_one` emits the enriched `TaskFinished` event on success too. With no
/// agent output the synthesized envelope summary is blank, so it is omitted.
#[cfg(unix)]
/// A user-started task (`interactive = true`) runs the agent's real TUI: the
/// rendered prompt goes through the interactive `prompt_args`, not
/// `headless_args`, and the retained session is not marked headless so the
/// Agent panel attaches to it writable.
#[cfg(unix)]
/// Regression: a user-started interactive task whose TUI never exits must still
/// complete once the agent finishes the seeded turn, so its `spawn` successors
/// are launched. The fake `opencode` stays alive for the TUI but answers the
/// turn-completion probe with a terminal `idle` message.
/// A programmatic task (`interactive = false`) keeps running headless with the
/// `headless_args`, preserving structured output / unattended completion.
#[cfg(unix)]
/// Regression: an isolated run reads its `spawn_file` from the worktree
/// *before* reclaiming it. The worktree used to be removed before the
/// handoff was read, so the manifest was already gone and the `spawn` tasks
/// were silently dropped.
/// A live interactive Agent-panel session attached to a task keeps the task's
/// worktree alive after its headless run exits (the session still runs inside
/// it).
#[cfg(unix)]
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
        config_path: std::path::PathBuf::new(),
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

/// A scratch directory for a git-backed executor test (not created).
fn scratch_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("favetto-{tag}-{}", Uuid::new_v4()))
}

/// Run `git -C <repo> <args>`, panicking on a non-zero exit.
async fn git_run(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Initialize a real git repository at `repo` with one unsigned commit.
async fn init_git_repo(repo: &Path) {
    std::fs::create_dir_all(repo).unwrap();
    git_run(repo, &["init", "-q"]).await;
    git_run(repo, &["config", "user.email", "t@example.com"]).await;
    git_run(repo, &["config", "user.name", "t"]).await;
    std::fs::write(repo.join("README.md"), "seed").unwrap();
    git_run(repo, &["add", "-A"]).await;
    git_run(
        repo,
        &["-c", "commit.gpgsign=false", "commit", "-qm", "init"],
    )
    .await;
}

/// Every local branch in `repo`, short-named.
async fn git_branches(repo: &Path) -> Vec<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["branch", "--list", "--format=%(refname:short)"])
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "git branch --list: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

/// A catalog task definition whose `cwd` is `repo`.
fn repo_def(name: &str, repo: &Path, extra: &str) -> TaskDef {
    crate::tasks::parse_task_md(
        name,
        &format!(
            "agent = \"x\"\ncwd = {:?}\n{extra}---\nbody\n",
            repo.to_string_lossy()
        ),
    )
    .unwrap()
}

/// The 8-hex suffix parser is the inverse of `branch_name`; keep it honest.
#[test]
fn parses_task_id_suffix_from_branch_name() {
    let task = lineage_task("pipelines/plan", TaskStatus::Pending, None, None);
    let short = task.id.to_string();
    assert_eq!(
        branch_suffix(&branch_name(&task)).map(str::to_string),
        Some(short[..8].to_string())
    );
    assert_eq!(branch_suffix("favetto/foo-deadbeef"), Some("deadbeef"));
    assert_eq!(branch_suffix("favetto/a-b-c-12345678"), Some("12345678"));
    assert_eq!(branch_suffix("main"), None);
    assert_eq!(branch_suffix("favetto/no-suffix"), None);
    assert_eq!(branch_suffix("favetto/foo-nothex!"), None);
    assert_eq!(branch_suffix("other/foo-deadbeef"), None);
}

/// Regression for #206: a linked worktree directory removed out-of-band leaves
/// git's registration behind, which used to make `git branch -D` fail and pin
/// the branch forever. `remove_worktree` prunes first, so the branch still goes.
#[tokio::test]
async fn remove_worktree_deletes_branch_after_directory_vanished() {
    let dir = scratch_dir("rm-vanished");
    let repo = dir.join("repo");
    init_git_repo(&repo).await;
    let state = join_state(&dir, Vec::new()).await;

    let task = lineage_task("review", TaskStatus::Succeeded, None, None);
    let branch = branch_name(&task);
    let worktree = dir.join("worktrees").join(task.id.to_string());
    std::fs::create_dir_all(worktree.parent().unwrap()).unwrap();
    let wt = worktree.to_string_lossy().into_owned();
    git_run(&repo, &["worktree", "add", "--force", "-B", &branch, &wt]).await;
    assert!(git_branches(&repo).await.contains(&branch));

    // Out-of-band cleanup: the directory is gone but git still has the row.
    std::fs::remove_dir_all(&worktree).unwrap();

    remove_worktree(&state.db, task.id, &repo, &worktree, &branch).await;

    let branches = git_branches(&repo).await;
    assert!(
        !branches.contains(&branch),
        "branch leaked after its worktree directory vanished: {branches:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression for #206: a `favetto/*` branch with no `worktrees` row (the
/// leaked backlog) is invisible to the row-based pass and must be reclaimed by
/// the git-native sweep.
#[tokio::test]
async fn prune_worktrees_reclaims_untracked_favetto_branch() {
    let dir = scratch_dir("sweep-untracked");
    let repo = dir.join("repo");
    init_git_repo(&repo).await;
    let state = join_state(&dir, vec![repo_def("review", &repo, "")]).await;

    let task = lineage_task("review", TaskStatus::Succeeded, None, None);
    db::insert_task(&state.db, &task).await.unwrap();
    let branch = branch_name(&task);
    git_run(&repo, &["branch", &branch]).await;
    assert!(git_branches(&repo).await.contains(&branch));

    let stats = prune_worktrees(&state).await.unwrap();
    assert!(stats.branches_removed >= 1, "stats: {stats:?}");
    let branches = git_branches(&repo).await;
    assert!(
        !branches.contains(&branch),
        "untracked favetto branch leaked: {branches:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The sweep must never touch a branch owned by a pending/running task.
#[tokio::test]
async fn prune_worktrees_keeps_active_task_branch() {
    let dir = scratch_dir("sweep-active");
    let repo = dir.join("repo");
    init_git_repo(&repo).await;
    let state = join_state(&dir, vec![repo_def("review", &repo, "")]).await;

    let pending = lineage_task("review", TaskStatus::Pending, None, None);
    let running = lineage_task("review", TaskStatus::Running, None, None);
    db::insert_task(&state.db, &pending).await.unwrap();
    db::insert_task(&state.db, &running).await.unwrap();
    let pending_branch = branch_name(&pending);
    let running_branch = branch_name(&running);
    git_run(&repo, &["branch", &pending_branch]).await;
    git_run(&repo, &["branch", &running_branch]).await;

    prune_worktrees(&state).await.unwrap();

    let branches = git_branches(&repo).await;
    assert!(
        branches.contains(&pending_branch),
        "pending task branch was reclaimed: {branches:?}"
    );
    assert!(
        branches.contains(&running_branch),
        "running task branch was reclaimed: {branches:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression for #206: a headless review-style run must not leak its
/// `favetto/*` branch or linked worktree after it finishes.
#[cfg(unix)]
#[tokio::test]
async fn review_run_does_not_leak_a_branch() {
    use std::os::unix::fs::PermissionsExt;

    let dir = scratch_dir("review-run");
    let repo = dir.join("repo");
    init_git_repo(&repo).await;

    let script = dir.join("fake-agent.sh");
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
    cfg.executor.parallel = true;

    let def = crate::tasks::parse_task_md(
        "review",
        &format!(
            "agent = \"opencode\"\ncwd = {:?}\n---\nReview.\n",
            repo.to_string_lossy()
        ),
    )
    .unwrap();
    let state = join_state_with_config(&dir, vec![def.clone()], cfg).await;

    let task = lineage_task("review", TaskStatus::Running, None, None);
    db::insert_task(&state.db, &task).await.unwrap();
    let plan = make_plan(
        &state,
        &state.config.executor,
        &repo,
        &task,
        def.worktree.unwrap_or(true),
    )
    .await
    .unwrap();
    assert!(plan.worktree.is_some(), "expected an isolated run");

    let run = running_run(task.id);
    db::insert_task_run(&state.db, &run).await.unwrap();
    run_one(&state, task.clone(), run, def, plan, None).await;

    let branches = git_branches(&repo).await;
    assert!(
        branches.iter().all(|b| !b.starts_with("favetto/")),
        "a finished review run leaked a branch: {branches:?}"
    );
    let worktree = dir.join("worktrees").join(task.id.to_string());
    assert!(
        !worktree.exists(),
        "a finished review run leaked its worktree"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression for #206: a task killed/cancelled with a linked worktree and no
/// tracking row is reclaimed on the next sweep.
#[tokio::test]
async fn cancelled_task_leaves_no_branch_or_worktree() {
    let dir = scratch_dir("sweep-cancelled");
    let repo = dir.join("repo");
    init_git_repo(&repo).await;
    let state = join_state(&dir, vec![repo_def("triage", &repo, "")]).await;

    let task = lineage_task("triage", TaskStatus::Cancelled, None, None);
    db::insert_task(&state.db, &task).await.unwrap();
    let branch = branch_name(&task);
    let worktree = dir.join("worktrees").join(task.id.to_string());
    std::fs::create_dir_all(worktree.parent().unwrap()).unwrap();
    let wt = worktree.to_string_lossy().into_owned();
    git_run(&repo, &["worktree", "add", "--force", "-B", &branch, &wt]).await;

    let stats = prune_worktrees(&state).await.unwrap();
    assert!(stats.untracked_worktrees_removed >= 1, "stats: {stats:?}");
    assert!(!worktree.exists(), "cancelled task worktree leaked");
    let branches = git_branches(&repo).await;
    assert!(
        !branches.contains(&branch),
        "cancelled task branch leaked: {branches:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A per-task `worktree = false` opt-out runs read-only tasks in the base dir
/// (serialized) and never creates a branch.
#[tokio::test]
async fn worktree_false_task_runs_in_base_dir_without_branch() {
    let dir = scratch_dir("worktree-false");
    let repo = dir.join("repo");
    init_git_repo(&repo).await;
    let def = repo_def("review", &repo, "worktree = false\n");
    let mut cfg = FavettoConfig::default();
    cfg.executor.parallel = true;
    let state = join_state_with_config(&dir, vec![def.clone()], cfg).await;

    let task = lineage_task("review", TaskStatus::Pending, None, None);
    let plan = make_plan(
        &state,
        &state.config.executor,
        &repo,
        &task,
        def.worktree.unwrap_or(true),
    )
    .await
    .unwrap();

    assert!(plan.worktree.is_none(), "worktree = false must opt out");
    assert_eq!(plan.cwd, repo);
    assert!(plan.needs_lock);
    assert!(
        git_branches(&repo)
            .await
            .iter()
            .all(|b| !b.starts_with("favetto/")),
        "worktree = false created a branch"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression for issue #91: resuming an agent session must run in the run's
/// worktree, and a worktree reclaimed by retention is recreated so a
/// per-project session store (pi) can find the session again.
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
        parent_id: parent,
        root_id: root,
        interactive: false,
    }
}

fn finished_event(name: &str, id: Uuid) -> Event {
    finished_event_at(1, name, id, true)
}

/// A `TaskFinished` event with an explicit id and outcome, so tests can tell
/// success from failure and control the `needs:` dedupe key.
fn finished_event_at(event_id: i64, name: &str, id: Uuid, success: bool) -> Event {
    Event {
        id: event_id,
        kind: EventKind::TaskFinished,
        payload: serde_json::json!({
            "name": name,
            "task_id": id.to_string(),
            "success": success,
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
async fn succeeded_dependency_fires_only_on_success() {
    let dir = std::env::temp_dir().join(format!("favetto-needs-ok-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = join_state(&dir, vec![needs_def("ok", "target:succeeded")]).await;

    let target = lineage_task("target", TaskStatus::Succeeded, None, None);
    db::insert_task(&state.db, &target).await.unwrap();

    // A failure outcome must not start a `:succeeded` dependent.
    start_dependents(&state, &finished_event_at(1, "target", target.id, false)).await;
    assert!(
        pending_named(&state, "ok").await.is_empty(),
        "`:succeeded` fired on a failed predecessor"
    );

    start_dependents(&state, &finished_event_at(2, "target", target.id, true)).await;
    assert_eq!(pending_named(&state, "ok").await.len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn failed_dependency_fires_only_on_failure() {
    let dir = std::env::temp_dir().join(format!("favetto-needs-bad-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = join_state(&dir, vec![needs_def("bad", "target:failed")]).await;

    let target = lineage_task("target", TaskStatus::Failed, None, None);
    db::insert_task(&state.db, &target).await.unwrap();

    // A success outcome must not start a `:failed` dependent.
    start_dependents(&state, &finished_event_at(1, "target", target.id, true)).await;
    assert!(
        pending_named(&state, "bad").await.is_empty(),
        "`:failed` fired on a successful predecessor"
    );

    start_dependents(&state, &finished_event_at(2, "target", target.id, false)).await;
    assert_eq!(pending_named(&state, "bad").await.len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn finished_and_terminal_dependencies_fire_on_failure() {
    let dir = std::env::temp_dir().join(format!("favetto-needs-terminal-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = join_state(
        &dir,
        vec![
            needs_def("legacy", "target"),
            needs_def("either", "target:terminal"),
        ],
    )
    .await;

    let target = lineage_task("target", TaskStatus::Failed, None, None);
    db::insert_task(&state.db, &target).await.unwrap();

    start_dependents(&state, &finished_event_at(1, "target", target.id, false)).await;
    assert_eq!(pending_named(&state, "legacy").await.len(), 1);
    assert_eq!(pending_named(&state, "either").await.len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

/// The enriched payload must not change `needs` semantics: `:finished` still
/// fires on a failure now that the event carries extra fields.
#[tokio::test]
async fn enriched_finished_payload_still_starts_dependents() {
    let dir = std::env::temp_dir().join(format!("favetto-finish-needs-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = join_state(&dir, vec![needs_def("follower", "target:finished")]).await;

    let mut target = lineage_task("target", TaskStatus::Failed, None, None);
    target.error = Some("agent failed".to_string());
    target.failure = Some(Failure::new(FailureKind::Agent, "agent failed"));
    db::insert_task(&state.db, &target).await.unwrap();

    let event = Event {
        id: 1,
        kind: EventKind::TaskFinished,
        payload: finished_payload(&target, false),
        created_at: Utc::now(),
    };
    start_dependents(&state, &event).await;

    assert_eq!(
        pending_named(&state, "follower").await.len(),
        1,
        "`:finished` did not start a dependent on an enriched failure payload"
    );

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
async fn finished_dependency_prev_bounds_large_envelope() {
    let dir = std::env::temp_dir().join(format!("favetto-needs-envelope-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = join_state(&dir, vec![needs_def("follower", "target:finished")]).await;

    let mut target = lineage_task("target", TaskStatus::Succeeded, None, None);
    let big = "x".repeat(200 * 1024);
    target.output = Some(serde_json::json!({
        "agent": "opencode",
        "output": "small transcript",
        "envelope": {
            "summary": "done",
            "artifacts": [{ "kind": "source", "path": big }],
            "findings": [{ "note": big }],
            "outputs": { "blob": big },
            "continuation": null,
        },
    }));
    db::insert_task(&state.db, &target).await.unwrap();

    start_dependents(&state, &finished_event("target", target.id)).await;
    let followers = pending_named(&state, "follower").await;
    assert_eq!(followers.len(), 1);
    let prev = &followers[0].input["_prev"];
    assert_eq!(prev["output"]["truncated"], true);
    let envelope = &prev["output"]["envelope"];
    assert!(envelope["artifacts"].is_string());
    assert!(envelope["findings"].is_string());
    assert!(envelope["outputs"].is_string());
    assert!(
        prev.to_string().len() < PREV_OUTPUT_BYTES + 16 * 1024,
        "single-dep envelope _prev is {} bytes",
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

#[tokio::test]
async fn enqueue_dynamic_persists_task_and_dependencies() {
    let dir = std::env::temp_dir().join(format!("favetto-dynamic-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = join_state(&dir, Vec::new()).await;

    let root = Uuid::new_v4();
    let predecessor = Uuid::new_v4();
    let id = Uuid::new_v4();
    let task = enqueue_dynamic(
        &state,
        DynamicNode {
            id,
            name: "child".to_string(),
            input: serde_json::json!({ "k": 1 }),
            dedupe_key: Some("workflow:op:child".to_string()),
            root_id: root,
            parent_id: Some(root),
            depends_on: vec![predecessor],
        },
    )
    .await
    .unwrap();

    assert_eq!(task.id, id, "the caller-assigned id must be preserved");
    assert_eq!(task.root_id, Some(root));
    assert_eq!(task.parent_id, Some(root));
    assert_eq!(task.status, TaskStatus::Pending);

    let stored = db::get_task(&state.db, id).await.unwrap().unwrap();
    assert_eq!(stored.name, "child");
    assert_eq!(
        db::list_dependencies(&state.db, id).await.unwrap(),
        vec![predecessor]
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn spawn_dynamic_records_dependencies_and_lineage() {
    let dir = std::env::temp_dir().join(format!("favetto-spawn-dyn-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = join_state(&dir, Vec::new()).await;

    let root = Uuid::new_v4();
    let predecessor = lineage_task("pred", TaskStatus::Succeeded, Some(root), Some(root));
    db::insert_task(&state.db, &predecessor).await.unwrap();

    let task = spawn_dynamic(
        &state,
        "child".to_string(),
        serde_json::json!({}),
        Some(root),
        &[predecessor.id],
        None,
    )
    .await
    .unwrap();

    assert_eq!(task.root_id, Some(root));
    assert_eq!(task.parent_id, Some(root));
    assert_eq!(
        db::list_dependencies(&state.db, task.id).await.unwrap(),
        vec![predecessor.id]
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn spawn_dynamic_with_dedupe_key_is_idempotent() {
    let dir = std::env::temp_dir().join(format!("favetto-spawn-dedupe-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = join_state(&dir, Vec::new()).await;

    let first = spawn_dynamic(
        &state,
        "child".to_string(),
        serde_json::json!({}),
        None,
        &[],
        Some("spawn:op:1".to_string()),
    )
    .await
    .unwrap();
    let second = spawn_dynamic(
        &state,
        "child".to_string(),
        serde_json::json!({}),
        None,
        &[],
        Some("spawn:op:1".to_string()),
    )
    .await
    .unwrap();

    assert_eq!(first.id, second.id, "re-submission returns the stored row");
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks WHERE name = 'child'")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(rows, 1);

    let _ = std::fs::remove_dir_all(&dir);
}

/// The startup reconciler marks the in-flight run `interrupted`, fails the
/// owning task under the default `stale_run = "fail"` policy, and is idempotent.
#[tokio::test]
async fn reconcile_fails_stale_run_by_default_and_is_idempotent() {
    let dir = std::env::temp_dir().join(format!("favetto-reconcile-fail-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = join_state(&dir, Vec::new()).await;

    let mut task = lineage_task("t", TaskStatus::Running, None, None);
    task.attempt = 1;
    db::insert_task(&state.db, &task).await.unwrap();
    db::insert_task_run(&state.db, &running_run(task.id))
        .await
        .unwrap();

    let report = reconcile(&state).await.unwrap();
    assert_eq!(report.runs_interrupted, 1);
    assert_eq!(report.tasks_failed, 1);
    assert_eq!(report.tasks_retried, 0);
    assert_eq!(report.tasks_invalidated, 0);

    let stored = db::get_task(&state.db, task.id).await.unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::Failed);
    assert_eq!(
        stored.error.as_deref(),
        Some("interrupted by daemon restart")
    );
    assert_eq!(
        stored.failure.as_ref().map(|f| f.kind),
        Some(FailureKind::Infrastructure)
    );

    // A restart with one running task yields exactly one interrupted run.
    let runs = db::list_task_runs(&state.db, task.id).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, RunStatus::Interrupted);

    // The second pass is a no-op.
    assert_eq!(reconcile(&state).await.unwrap(), ReconcileReport::default());

    let _ = std::fs::remove_dir_all(&dir);
}

/// `stale_run = "retry"` re-enqueues the task for a fresh attempt while still
/// recording the old run as interrupted.
#[tokio::test]
async fn reconcile_retry_policy_re_enqueues_the_task() {
    let dir = std::env::temp_dir().join(format!("favetto-reconcile-retry-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = FavettoConfig::default();
    cfg.executor.stale_run = StaleRunPolicy::Retry;
    // The catalog must still define `t`, or the retried task is invalidated
    // instead of re-enqueued (covered by the vanished-definition test).
    let state = join_state_with_config(&dir, vec![def_with_vars()], cfg).await;

    let mut task = lineage_task("t", TaskStatus::AwaitingInput, None, None);
    task.attempt = 1;
    task.error = Some("previous error".to_string());
    db::insert_task(&state.db, &task).await.unwrap();
    db::insert_task_run(&state.db, &running_run(task.id))
        .await
        .unwrap();

    let report = reconcile(&state).await.unwrap();
    assert_eq!(report.runs_interrupted, 1);
    assert_eq!(report.tasks_retried, 1);
    assert_eq!(report.tasks_failed, 0);
    assert_eq!(report.tasks_invalidated, 0);

    let stored = db::get_task(&state.db, task.id).await.unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::Pending);
    assert!(stored.error.is_none() && stored.failure.is_none());
    assert_eq!(
        db::list_task_runs(&state.db, task.id).await.unwrap()[0].status,
        RunStatus::Interrupted
    );

    // Idempotent: the pending task is not touched again.
    assert_eq!(reconcile(&state).await.unwrap(), ReconcileReport::default());

    let _ = std::fs::remove_dir_all(&dir);
}

/// Pending tasks whose catalog definition vanished fail eagerly with
/// `InvalidInput`; a still-defined pending task is left for the executor.
#[tokio::test]
async fn reconcile_fails_pending_tasks_with_a_vanished_catalog_definition() {
    let dir = std::env::temp_dir().join(format!("favetto-reconcile-gone-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = join_state(&dir, vec![def_with_vars()]).await;

    let kept = lineage_task("t", TaskStatus::Pending, None, None);
    let gone = lineage_task("gone", TaskStatus::Pending, None, None);
    db::insert_task(&state.db, &kept).await.unwrap();
    db::insert_task(&state.db, &gone).await.unwrap();

    let report = reconcile(&state).await.unwrap();
    assert_eq!(report.tasks_invalidated, 1);
    assert_eq!(report.tasks_failed, 0);
    assert_eq!(report.runs_interrupted, 0);

    assert_eq!(
        db::get_task(&state.db, kept.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Pending
    );
    let stored = db::get_task(&state.db, gone.id).await.unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::Failed);
    assert_eq!(
        stored.failure.as_ref().map(|f| f.kind),
        Some(FailureKind::InvalidInput)
    );

    assert_eq!(reconcile(&state).await.unwrap(), ReconcileReport::default());

    let _ = std::fs::remove_dir_all(&dir);
}

/// A repo-local task defaults to its repo root as the base directory, and a
/// worktree-isolated run targets that repo's git toplevel.
#[tokio::test]
async fn repo_task_runs_in_and_branches_the_repo() {
    let dir = scratch_dir("repo-base-global");
    std::fs::create_dir_all(&dir).unwrap();
    let repo = scratch_dir("repo-base-checkout");
    init_git_repo(&repo).await;
    std::fs::create_dir_all(repo.join(".favetto/tasks")).unwrap();
    std::fs::write(
        repo.join(".favetto/tasks/triage.md"),
        "agent = \"x\"\n---\nbody\n",
    )
    .unwrap();

    let mut cfg = FavettoConfig::default();
    cfg.executor.parallel = true;
    let state = join_state_with_config(&dir, Vec::new(), cfg).await;
    crate::repos::add(
        &state,
        favetto_core::rpc::ReposAddParams {
            path: repo.to_string_lossy().into_owned(),
            name: Some("myrepo".to_string()),
        },
    )
    .await
    .unwrap();

    let def = state
        .catalog
        .read()
        .iter()
        .find(|d| d.name == "repo:myrepo/triage")
        .cloned()
        .expect("repo task loaded");
    // The loader defaulted `cwd` to the repo root.
    assert_eq!(def.cwd.as_deref(), Some(repo.to_string_lossy().as_ref()));

    let task = lineage_task(&def.name, TaskStatus::Pending, None, None);
    let base = resolve_base_dir(&def, &task);
    assert_eq!(base, repo);

    let plan = make_plan(&state, &state.config.executor, &base, &task, true)
        .await
        .unwrap();
    let worktree = plan.worktree.expect("parallel task gets a worktree");
    assert_eq!(
        worktree.repo,
        std::fs::canonicalize(&repo).unwrap(),
        "worktree must belong to the repo checkout"
    );
    assert_eq!(worktree.branch, branch_name(&task));

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&repo);
}

/// A one-shot local HTTP server returning `body` as `application/json` with a
/// chosen status line.
async fn serve_json_status_once(
    status: u16,
    body: &'static str,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = vec![0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let reason = http::StatusCode::from_u16(status)
                .ok()
                .and_then(|s| s.canonical_reason())
                .unwrap_or("");
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        }
    });
    (addr, handle)
}

/// A one-shot local HTTP server returning `body` as `application/json`.
async fn serve_json_once(
    body: &'static str,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    serve_json_status_once(200, body).await
}

/// A `kind = "action"` task runs its built-in action without an agent and folds
/// the result into the agent-shaped `task.output.envelope` (with `agent: null`).
#[tokio::test]
async fn run_one_runs_an_action_task_without_an_agent() {
    let dir = scratch_dir("action-envelope");
    std::fs::create_dir_all(&dir).unwrap();
    let (addr, server) = serve_json_once(r#"[{"number":1},{"number":2},{"number":3}]"#).await;
    let state = join_state(&dir, Vec::new()).await;

    let def = crate::tasks::parse_task_md(
        "gate",
        &format!(
            "kind = \"action\"\nworktree = false\n\
             [action]\nid = \"http.request\"\n\
             [action.params]\nurl = \"http://{addr}/issues\"\n\
             [action.when]\npath = \"items\"\nop = \"not_empty\"\n---\nPoll.\n"
        ),
    )
    .unwrap();
    assert_eq!(def.agent, None);

    let task = lineage_task("gate", TaskStatus::Running, None, None);
    db::insert_task(&state.db, &task).await.unwrap();
    let plan = Plan {
        cwd: dir.clone(),
        needs_lock: false,
        worktree: None,
    };
    let run = running_run(task.id);
    db::insert_task_run(&state.db, &run).await.unwrap();
    run_one(&state, task.clone(), run, def, plan, None).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;

    let stored = db::get_task(&state.db, task.id).await.unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::Succeeded, "{:?}", stored.error);
    let output = stored.output.as_ref().unwrap();
    assert_eq!(output["agent"], serde_json::Value::Null);
    assert_eq!(output["envelope"]["outputs"]["status"], 200);
    assert_eq!(
        output["envelope"]["outputs"]["items"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert!(
        output["envelope"]["summary"]
            .as_str()
            .unwrap()
            .contains("-> 200"),
        "{output}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A true `[action.when]` predicate writes the action's child manifest and
/// enqueues the child with the gate run's lineage.
#[tokio::test]
async fn action_when_true_writes_the_manifest_and_spawns_a_child() {
    let dir = scratch_dir("action-spawn-true");
    std::fs::create_dir_all(&dir).unwrap();
    let (addr, server) = serve_json_once(r#"[{"number":1}]"#).await;
    let state = join_state(&dir, Vec::new()).await;

    let def = crate::tasks::parse_task_md(
        "gate",
        &format!(
            "kind = \"action\"\nspawn = \"child\"\nspawn_file = \"handoff.json\"\n\
             [action]\nid = \"http.request\"\n\
             [action.params]\nurl = \"http://{addr}/issues\"\n\
             [action.spawn_input]\nissue_id = 7\n\
             [action.when]\npath = \"items\"\nop = \"not_empty\"\n---\nPoll.\n"
        ),
    )
    .unwrap();

    let root = Uuid::new_v4();
    let task = lineage_task("gate", TaskStatus::Running, Some(root), None);
    db::insert_task(&state.db, &task).await.unwrap();
    let plan = Plan {
        cwd: dir.clone(),
        needs_lock: false,
        worktree: None,
    };
    let run = running_run(task.id);
    db::insert_task_run(&state.db, &run).await.unwrap();
    run_one(&state, task.clone(), run, def, plan, None).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;

    assert!(
        dir.join("handoff.json").exists(),
        "manifest was not written"
    );
    let children = pending_named(&state, "child").await;
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].input["issue_id"], 7);
    assert_eq!(children[0].parent_id, Some(task.id));
    assert_eq!(children[0].root_id, Some(root));

    let _ = std::fs::remove_dir_all(&dir);
}

/// A manifest that cannot be written by an action task still leaves a durable
/// failed child row under the succeeded parent, so both clients can warn.
#[tokio::test]
async fn spawn_manifest_failure_is_recorded_as_a_failed_child_row() {
    let dir = scratch_dir("spawn-manifest-failure");
    std::fs::create_dir_all(&dir).unwrap();
    // A regular file where the manifest's parent directory must go makes
    // `create_dir_all` fail deterministically.
    std::fs::write(dir.join("blocker"), b"not a directory").unwrap();
    let (addr, server) = serve_json_once(r#"[{"number":1}]"#).await;
    let state = join_state(&dir, Vec::new()).await;

    let def = crate::tasks::parse_task_md(
        "gate",
        &format!(
            "kind = \"action\"\nworktree = false\nspawn = \"repo:Favetto/plan_issue\"\n\
             spawn_file = \"blocker/handoff.json\"\n\
             [action]\nid = \"http.request\"\n\
             [action.params]\nurl = \"http://{addr}/issues\"\n---\nPoll.\n"
        ),
    )
    .unwrap();

    let root = Uuid::new_v4();
    let task = lineage_task("gate", TaskStatus::Running, Some(root), None);
    db::insert_task(&state.db, &task).await.unwrap();
    let plan = Plan {
        cwd: dir.clone(),
        needs_lock: false,
        worktree: None,
    };
    let run = running_run(task.id);
    db::insert_task_run(&state.db, &run).await.unwrap();
    run_one(&state, task.clone(), run, def, plan, None).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;

    let parent = db::get_task(&state.db, task.id).await.unwrap().unwrap();
    assert_eq!(parent.status, TaskStatus::Succeeded);

    let children = db::list_tasks_in_root(&state.db, root, "repo:Favetto/plan_issue")
        .await
        .unwrap();
    assert_eq!(children.len(), 1, "spawn failure was not persisted");
    let child = &children[0];
    assert_eq!(child.status, TaskStatus::Failed);
    assert_eq!(child.parent_id, Some(task.id));
    assert_eq!(child.root_id, Some(root));
    assert_eq!(child.attempt, 1);
    assert!(
        child
            .error
            .as_deref()
            .is_some_and(|error| error.contains("spawn manifest preparation failed")),
        "{:?}",
        child.error
    );
    let failure = child.failure.as_ref().expect("typed failure");
    assert_eq!(failure.kind, FailureKind::Infrastructure);
    assert!(!failure.retryable, "a decided fan-out must not auto-retry");
    assert!(
        db::tail_events(&state.db, 20)
            .await
            .unwrap()
            .iter()
            .any(|event| event.kind == EventKind::TaskFinished
                && event.payload["task_id"] == child.id.to_string()),
        "the failure is not durable on the event bus"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A false `[action.when]` predicate still succeeds but enqueues nothing and
/// writes no manifest, matching an empty `[]` handoff.
#[tokio::test]
async fn action_when_false_enqueues_nothing_but_succeeds() {
    let dir = scratch_dir("action-spawn-false");
    std::fs::create_dir_all(&dir).unwrap();
    let (addr, server) = serve_json_once(r#"[{"number":1}]"#).await;
    let state = join_state(&dir, Vec::new()).await;

    let def = crate::tasks::parse_task_md(
        "gate",
        &format!(
            "kind = \"action\"\nspawn = \"child\"\nspawn_file = \"handoff.json\"\n\
             [action]\nid = \"http.request\"\n\
             [action.params]\nurl = \"http://{addr}/issues\"\n\
             [action.when]\npath = \"items\"\nop = \"empty\"\n---\nPoll.\n"
        ),
    )
    .unwrap();

    let task = lineage_task("gate", TaskStatus::Running, None, None);
    db::insert_task(&state.db, &task).await.unwrap();
    let plan = Plan {
        cwd: dir.clone(),
        needs_lock: false,
        worktree: None,
    };
    let run = running_run(task.id);
    db::insert_task_run(&state.db, &run).await.unwrap();
    run_one(&state, task.clone(), run, def, plan, None).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;

    let stored = db::get_task(&state.db, task.id).await.unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::Succeeded, "{:?}", stored.error);
    assert!(
        !dir.join("handoff.json").exists(),
        "manifest must not exist"
    );
    assert!(pending_named(&state, "child").await.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

/// A non-2xx `http.request` fails the action task and writes no child manifest,
/// even when `[action.when]` would otherwise have gated the spawn open.
#[tokio::test]
async fn action_task_fails_on_a_non_2xx_response_and_writes_no_manifest() {
    let dir = scratch_dir("action-non-2xx");
    std::fs::create_dir_all(&dir).unwrap();
    let (addr, server) = serve_json_status_once(403, r#"{"message":"Forbidden"}"#).await;
    let state = join_state(&dir, Vec::new()).await;

    let def = crate::tasks::parse_task_md(
        "gate",
        &format!(
            "kind = \"action\"\nspawn = \"child\"\nspawn_file = \"handoff.json\"\n\
             [action]\nid = \"http.request\"\n\
             [action.params]\nurl = \"http://{addr}/issues\"\n\
             [action.when]\npath = \"items\"\nop = \"not_empty\"\n---\nPoll.\n"
        ),
    )
    .unwrap();

    let task = lineage_task("gate", TaskStatus::Running, None, None);
    db::insert_task(&state.db, &task).await.unwrap();
    let plan = Plan {
        cwd: dir.clone(),
        needs_lock: false,
        worktree: None,
    };
    let run = running_run(task.id);
    db::insert_task_run(&state.db, &run).await.unwrap();
    run_one(&state, task.clone(), run, def, plan, None).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;

    let stored = db::get_task(&state.db, task.id).await.unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::Failed, "{:?}", stored.output);
    let failure = stored.failure.expect("a populated failure");
    assert_eq!(
        failure.kind,
        favetto_core::model::FailureKind::Infrastructure
    );
    assert!(stored.error.unwrap().contains("403"));
    assert!(
        !dir.join("handoff.json").exists(),
        "manifest must not exist"
    );
    assert!(pending_named(&state, "child").await.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}
