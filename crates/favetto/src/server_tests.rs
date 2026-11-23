use super::*;
use favetto_core::rpc::error_code;
use proptest::prelude::*;
use std::path::Path;
use tower::ServiceExt as _;

use crate::state::StateInit;

/// Deserialize a JSON params object for a direct helper call in a test.
fn json_params<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> T {
    serde_json::from_value(value).expect("test params")
}

/// A unique scratch directory for server tests.
fn temp_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "favetto-server-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A `State` backed by a scratch SQLite database and the given tasks dir.
async fn test_state(dir: &Path, tasks_dir: &Path) -> Arc<State> {
    test_state_with_config(dir, tasks_dir, crate::config::FavettoConfig::default()).await
}

/// A [`test_state`] with an explicit daemon config (e.g. a `[repos].base_dir`).
async fn test_state_with_config(
    dir: &Path,
    tasks_dir: &Path,
    config: crate::config::FavettoConfig,
) -> Arc<State> {
    let pool = crate::db::open(&dir.join("test.db")).await.unwrap();
    crate::db::migrate(&pool).await.unwrap();
    let catalog = Arc::new(parking_lot::RwLock::new(
        crate::tasks::load_catalog(tasks_dir).unwrap(),
    ));
    let scheduler = crate::scheduler::Scheduler::new().await.unwrap();
    // Use the same registry construction as the daemon, so a test config that
    // adds an agent can actually resolve it from `agents.history`/`agents.load`.
    let registry = crate::agents::AgentRegistry::from_config(&config).unwrap();
    let config = Arc::new(config);
    Arc::new(State::new(StateInit {
        db: pool,
        bus: crate::event_bus::EventBus::new(64),
        token: favetto_core::auth::Token::generate(),
        webhooks: crate::webhooks::WebhookSecrets::from_config(&config),
        agents: crate::agents::AgentManager::new(),
        registry,
        config,
        config_path: dir.join("config.toml"),
        data_dir: dir.to_path_buf(),
        tasks_dir: tasks_dir.to_path_buf(),
        catalog,
        scheduler,
        hook_store: Arc::new(parking_lot::RwLock::new(Vec::new())),
    }))
}

#[tokio::test]
async fn config_validate_reports_a_malformed_file() {
    let dir = temp_dir("config-validate-invalid");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let state = test_state(&dir, &tasks_dir).await;
    std::fs::write(dir.join("config.toml"), "not valid toml [[[").unwrap();

    let resp = dispatch(
        &state,
        Request {
            id: 1,
            method: method::CONFIG_VALIDATE.to_string(),
            params: serde_json::json!({}),
        },
    )
    .await;
    let result = resp.result.expect("config.validate result");
    assert_eq!(result["valid"], serde_json::json!(false));
    let path = result["path"].as_str().unwrap();
    assert!(path.ends_with("config.toml"), "{path}");
    let errors = result["errors"].as_array().expect("errors array");
    assert!(!errors.is_empty(), "expected at least one parse error");
}

#[tokio::test]
async fn config_validate_accepts_a_missing_file() {
    let dir = temp_dir("config-validate-missing");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    // No config.toml on disk: defaults apply, so validation stays silent.
    let state = test_state(&dir, &tasks_dir).await;

    let resp = dispatch(
        &state,
        Request {
            id: 1,
            method: method::CONFIG_VALIDATE.to_string(),
            params: serde_json::json!({}),
        },
    )
    .await;
    let result = resp.result.expect("config.validate result");
    assert_eq!(result["valid"], serde_json::json!(true));
    assert_eq!(result["errors"], serde_json::json!([]));
    assert!(result["path"].as_str().unwrap().ends_with("config.toml"));
}

#[tokio::test]
async fn workflow_get_returns_dot_and_path() {
    let dir = temp_dir("workflow-get");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(
        tasks_dir.join("a.md"),
        "agent = \"x\"\nspawn = \"b\"\n---\nprompt a\n",
    )
    .unwrap();
    let state = test_state(&dir, &tasks_dir).await;

    let req = Request {
        id: 1,
        method: method::WORKFLOW_GET.to_string(),
        params: serde_json::json!({}),
    };
    let resp = dispatch(&state, req).await;
    let result = resp.result.expect("workflow.get result");
    let dot = result.get("dot").and_then(|d| d.as_str()).unwrap();
    assert!(dot.contains("\"a\" -> \"b\" [label=\"spawn\"];"), "{dot}");
    let path = result.get("path").and_then(|p| p.as_str()).unwrap();
    assert!(path.ends_with("workflow.dot"), "{path}");

    let graph = result.get("graph").expect("graph field");
    let nodes = graph
        .get("nodes")
        .and_then(|n| n.as_array())
        .expect("graph nodes");
    assert!(
        nodes
            .iter()
            .any(|n| n["name"] == "a" && n["scheduled"] == false && n["external"] == false),
        "{graph}"
    );
    assert!(
        nodes
            .iter()
            .any(|n| n["name"] == "b" && n["external"] == true),
        "{graph}"
    );
    let edges = graph
        .get("edges")
        .and_then(|e| e.as_array())
        .expect("graph edges");
    assert_eq!(
        edges[0],
        serde_json::json!({ "from": "a", "to": "b", "kind": "spawn" })
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn usage_stats_dispatches_aggregated_usage() {
    use favetto_core::agent_state::{AgentUsage, UsagePeriod};
    use favetto_core::model::{RunStatus, TaskRun};

    let dir = temp_dir("usage-stats");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let state = test_state(&dir, &tasks_dir).await;

    let now = Timestamp::now();
    let task_id = Uuid::new_v4();
    db::insert_task_run(
        &state.db,
        &TaskRun {
            id: Uuid::new_v4(),
            task_id,
            attempt: 1,
            status: RunStatus::Succeeded,
            agent: Some("opencode".to_string()),
            model: Some("deepseek-v4-flash".to_string()),
            session_id: None,
            started_at: Some(now),
            finished_at: Some(now),
            exit_code: Some(0),
            error: None,
            usage: Some(AgentUsage {
                input_tokens: 120,
                output_tokens: 30,
                cost_usd: Some(0.42),
                ..Default::default()
            }),
            failure: None,
        },
    )
    .await
    .unwrap();

    let resp = dispatch(
        &state,
        Request {
            id: 1,
            method: method::USAGE_STATS.to_string(),
            params: serde_json::json!({ "period": "month" }),
        },
    )
    .await;
    let result = resp.result.expect("usage.stats result");
    assert_eq!(result["period"], UsagePeriod::Month.as_str());
    assert_eq!(result["totals"]["usage"]["input_tokens"], 120);
    assert_eq!(result["totals"]["usage"]["output_tokens"], 30);
    assert_eq!(result["totals"]["usage"]["cost_usd"], 0.42);
    assert_eq!(result["totals"]["runs"], 1);
    let buckets = result["buckets"].as_array().expect("buckets");
    assert_eq!(buckets.len(), 30);

    // A missing period defaults to `day`.
    let resp = dispatch(
        &state,
        Request {
            id: 2,
            method: method::USAGE_STATS.to_string(),
            params: serde_json::json!({}),
        },
    )
    .await;
    let result = resp.result.expect("usage.stats result");
    assert_eq!(result["period"], "day");
    assert_eq!(result["buckets"].as_array().map(Vec::len), Some(24));

    let _ = std::fs::remove_dir_all(&dir);
}

/// Decode a `workflow.inspect` id bucket into `Uuid`s.
fn id_list(result: &serde_json::Value, field: &str) -> Vec<Uuid> {
    result[field]
        .as_array()
        .unwrap_or_else(|| panic!("{field} must be an array"))
        .iter()
        .map(|v| Uuid::parse_str(v.as_str().expect("uuid string")).unwrap())
        .collect()
}

#[tokio::test]
async fn workflow_inspect_buckets_a_spawn_all_finished_root() {
    let dir = temp_dir("workflow-inspect");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(tasks_dir.join("root.md"), "agent = \"x\"\n---\nroot\n").unwrap();
    std::fs::write(tasks_dir.join("child.md"), "agent = \"x\"\n---\nchild\n").unwrap();
    std::fs::write(
        tasks_dir.join("join.md"),
        "agent = \"x\"\nneeds = \"child:all_finished\"\n---\njoin\n",
    )
    .unwrap();
    let state = test_state(&dir, &tasks_dir).await;

    let root = Uuid::new_v4();
    let mut root_task = oneshot_task();
    root_task.id = root;
    root_task.name = "root".to_string();
    root_task.root_id = None;

    let mut child_fail = oneshot_task();
    child_fail.name = "child".to_string();
    child_fail.status = TaskStatus::Failed;
    child_fail.finished_at = Some(Timestamp::now());
    child_fail.error = Some("boom".to_string());
    child_fail.root_id = Some(root);
    child_fail.parent_id = Some(root);

    let mut child_open = oneshot_task();
    child_open.name = "child".to_string();
    child_open.root_id = Some(root);
    child_open.parent_id = Some(root);

    let mut join = oneshot_task();
    join.name = "join".to_string();
    join.status = TaskStatus::Pending;
    join.started_at = None;
    join.root_id = Some(root);
    join.parent_id = Some(root);

    for task in [&root_task, &child_fail, &child_open, &join] {
        db::upsert_task(&state.db, task).await.unwrap();
    }

    let resp = dispatch(
        &state,
        Request {
            id: 1,
            method: method::WORKFLOW_INSPECT.to_string(),
            params: serde_json::json!({ "root_id": root }),
        },
    )
    .await;
    let result = resp.result.expect("workflow.inspect result");

    assert_eq!(result["root_task"], "root");
    assert_eq!(result["state"], "running");
    let tasks = result["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 4);
    assert!(
        tasks.iter().all(|t| t.get("output").is_none()),
        "runtime view must not carry output blobs: {tasks:?}"
    );
    let mut running = id_list(&result, "running");
    running.sort();
    let mut expected_running = vec![root, child_open.id];
    expected_running.sort();
    assert_eq!(running, expected_running);
    assert_eq!(id_list(&result, "failed"), vec![child_fail.id]);
    assert_eq!(id_list(&result, "blocked"), vec![join.id]);
    assert!(id_list(&result, "ready").is_empty());

    // The failed task surfaces its error as the bounded summary.
    let failed_view = tasks
        .iter()
        .find(|t| t["id"] == child_fail.id.to_string())
        .expect("failed child view");
    assert_eq!(failed_view["summary"], "boom");
    assert_eq!(failed_view["attempt"], 1);

    // Once the last active child finishes, the all_finished barrier unblocks.
    let mut child_done = child_open.clone();
    child_done.status = TaskStatus::Succeeded;
    child_done.finished_at = Some(Timestamp::now());
    db::upsert_task(&state.db, &child_done).await.unwrap();

    let resp = dispatch(
        &state,
        Request {
            id: 2,
            method: method::WORKFLOW_INSPECT.to_string(),
            params: serde_json::json!({ "root_id": root }),
        },
    )
    .await;
    let result = resp.result.expect("second workflow.inspect result");
    assert!(id_list(&result, "blocked").is_empty());
    assert_eq!(id_list(&result, "ready"), vec![join.id]);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn workflow_inspect_unknown_root_is_invalid_params() {
    let dir = temp_dir("workflow-inspect-missing");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let state = test_state(&dir, &tasks_dir).await;

    let resp = dispatch(
        &state,
        Request {
            id: 1,
            method: method::WORKFLOW_INSPECT.to_string(),
            params: serde_json::json!({ "root_id": Uuid::new_v4() }),
        },
    )
    .await;
    assert!(resp.result.is_none());
    let error = resp.error.expect("unknown root must error");
    assert_eq!(error.code, error_code::INVALID_PARAMS);
    assert!(
        error.message.contains("workflow root not found"),
        "{error:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `workflow.cancel` cancels every non-terminal task in the root — including a
/// task that is mid-run — and leaves terminal tasks untouched. Each cancelled
/// task emits `TaskCancelled`, no `TaskFinished` fires, so `needs`/join
/// listeners never schedule follow-on work.
#[tokio::test]
async fn workflow_cancel_cancels_active_tasks_and_skips_terminal() {
    let dir = temp_dir("workflow-cancel");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(tasks_dir.join("root.md"), "agent = \"x\"\n---\nroot\n").unwrap();
    std::fs::write(
        tasks_dir.join("dependent.md"),
        "agent = \"x\"\nneeds = \"root:finished\"\n---\ndependent\n",
    )
    .unwrap();
    let state = test_state(&dir, &tasks_dir).await;

    let root = Uuid::new_v4();
    let mut root_task = oneshot_task();
    root_task.id = root;
    root_task.name = "root".to_string();
    root_task.root_id = None;

    // A task mid-run (cancel-during-run) and a pending descendant.
    let mut child_running = oneshot_task();
    child_running.name = "child".to_string();
    child_running.root_id = Some(root);

    let mut grandchild_pending = oneshot_task();
    grandchild_pending.name = "grandchild".to_string();
    grandchild_pending.status = TaskStatus::Pending;
    grandchild_pending.started_at = None;
    grandchild_pending.root_id = Some(root);

    // An already-terminal task must not be re-cancelled.
    let mut done = oneshot_task();
    done.name = "done".to_string();
    done.status = TaskStatus::Succeeded;
    done.finished_at = Some(Timestamp::now());
    done.root_id = Some(root);

    for task in [&root_task, &child_running, &grandchild_pending, &done] {
        db::upsert_task(&state.db, task).await.unwrap();
    }

    let resp = dispatch(
        &state,
        Request {
            id: 1,
            method: method::WORKFLOW_CANCEL.to_string(),
            params: serde_json::json!({ "root_id": root }),
        },
    )
    .await;
    let result = resp.result.expect("workflow.cancel result");
    assert_eq!(result["root_id"], root.to_string());
    let mut cancelled = id_list(&result, "cancelled");
    cancelled.sort();
    let mut expected = vec![root, child_running.id, grandchild_pending.id];
    expected.sort();
    assert_eq!(cancelled, expected);
    assert!(
        !cancelled.contains(&done.id),
        "a terminal task is never cancelled"
    );

    for task in [&root_task, &child_running, &grandchild_pending] {
        let got = db::get_task(&state.db, task.id).await.unwrap().unwrap();
        assert_eq!(got.status, TaskStatus::Cancelled, "{}", task.name);
        assert!(got.finished_at.is_some());
    }
    let stored_done = db::get_task(&state.db, done.id).await.unwrap().unwrap();
    assert_eq!(stored_done.status, TaskStatus::Succeeded);

    // One durable `TaskCancelled` event per cancelled task, and no `TaskFinished`
    // that could trigger a dependent or fan-in.
    let events = db::tail_events(&state.db, 50).await.unwrap();
    let cancelled_events = events
        .iter()
        .filter(|e| e.kind == EventKind::TaskCancelled)
        .count();
    assert_eq!(cancelled_events, expected.len());
    assert!(
        !events.iter().any(|e| e.kind == EventKind::TaskFinished),
        "cancel must not fire follow-on scheduling: {events:?}"
    );

    // No new task was enqueued for the `needs` dependent.
    let names: Vec<String> = db::list_root_tasks(&state.db, root)
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(names.len(), 4, "{names:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn workflow_cancel_unknown_root_is_invalid_params() {
    let dir = temp_dir("workflow-cancel-missing");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let state = test_state(&dir, &tasks_dir).await;

    let resp = dispatch(
        &state,
        Request {
            id: 1,
            method: method::WORKFLOW_CANCEL.to_string(),
            params: serde_json::json!({ "root_id": Uuid::new_v4() }),
        },
    )
    .await;
    assert!(resp.result.is_none());
    let error = resp.error.expect("unknown root must error");
    assert_eq!(error.code, error_code::INVALID_PARAMS);
    assert!(
        error.message.contains("workflow root not found"),
        "{error:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `tasks.cancel` on a running task flips it to `cancelled`, terminates its live
/// agent session, and emits exactly one `TaskCancelled`. Already-terminal tasks
/// are returned untouched with no second event.
#[cfg(unix)]
/// `workflow.cancel` terminates the live agent session of every mid-run task it
/// cancels, not just flips the row.
#[cfg(unix)]
/// `workflow.retry` delegates to the manual retry, keyed by `task_id`.
#[tokio::test]
async fn workflow_retry_delegates_to_manual_retry() {
    use favetto_core::model::{Failure, FailureKind, RunStatus, TaskRun};

    let dir = temp_dir("workflow-retry");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let state = test_state(&dir, &tasks_dir).await;

    let mut failed = oneshot_task();
    failed.name = "failed".to_string();
    failed.status = TaskStatus::Failed;
    failed.error = Some("boom".to_string());
    failed.failure = Some(Failure::new(FailureKind::Agent, "boom"));
    failed.finished_at = Some(Timestamp::now());
    db::upsert_task(&state.db, &failed).await.unwrap();
    db::insert_task_run(
        &state.db,
        &TaskRun {
            id: Uuid::new_v4(),
            task_id: failed.id,
            attempt: 1,
            status: RunStatus::Failed,
            agent: None,
            model: None,
            session_id: None,
            started_at: Some(Timestamp::now()),
            finished_at: Some(Timestamp::now()),
            exit_code: None,
            error: None,
            usage: None,
            failure: None,
        },
    )
    .await
    .unwrap();

    let resp = dispatch(
        &state,
        Request {
            id: 1,
            method: method::WORKFLOW_RETRY.to_string(),
            params: serde_json::json!({ "task_id": failed.id }),
        },
    )
    .await;
    let result = resp.result.expect("workflow.retry result");
    assert_eq!(result["status"], "pending");
    assert_eq!(
        db::list_task_runs(&state.db, failed.id)
            .await
            .unwrap()
            .len(),
        1,
        "manual retry preserves run history"
    );

    // Unknown id is invalid params, not internal.
    let resp = dispatch(
        &state,
        Request {
            id: 2,
            method: method::WORKFLOW_RETRY.to_string(),
            params: serde_json::json!({ "task_id": Uuid::new_v4() }),
        },
    )
    .await;
    assert_eq!(resp.error.unwrap().code, error_code::INVALID_PARAMS);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn add_catalog_task_writes_dot_and_pushes_catalog_updated() {
    let dir = temp_dir("add");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let state = test_state(&dir, &tasks_dir).await;
    let mut rx = state.bus.subscribe();

    let params = serde_json::json!({
        "name": "newtask",
        "agent": "x",
        "prompt": "hello",
    });
    let def = add_catalog_task(&state, json_params(params.clone()))
        .await
        .unwrap();
    assert_eq!(def.name, "newtask");

    let dot = std::fs::read_to_string(dir.join("workflow.dot")).unwrap();
    assert!(dot.contains("\"newtask\" [label=\"newtask\"];"), "{dot}");
    assert!(matches!(rx.try_recv(), Ok(ServerPush::CatalogUpdated)));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn add_catalog_task_dot_reflects_spawn() {
    let dir = temp_dir("add-spawn");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(tasks_dir.join("child.md"), "agent = \"x\"\n---\nbody\n").unwrap();
    let state = test_state(&dir, &tasks_dir).await;

    let params = serde_json::json!({
        "name": "parent",
        "agent": "x",
        "spawn": "child",
    });
    add_catalog_task(&state, json_params(params.clone()))
        .await
        .unwrap();

    let dot = std::fs::read_to_string(dir.join("workflow.dot")).unwrap();
    assert!(
        dot.contains("\"parent\" -> \"child\" [label=\"spawn\"];"),
        "{dot}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn add_catalog_task_with_schedule_registers_cron() {
    let dir = temp_dir("add-schedule");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let state = test_state(&dir, &tasks_dir).await;

    let params = serde_json::json!({
        "name": "scheduled",
        "agent": "x",
        "prompt": "hello",
        "schedule": "0 0 8 * * *",
    });
    add_catalog_task(&state, json_params(params.clone()))
        .await
        .unwrap();

    let scheduled = crate::db::list_schedules(&state.db).await.unwrap();
    let entry = scheduled
        .iter()
        .find(|s| s.id == "catalog:scheduled")
        .expect("catalog schedule registered on add");
    assert_eq!(entry.cron, "0 0 8 * * *");
    assert_eq!(entry.task, "scheduled");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn schedules_list_reports_next_run_for_enabled_only() {
    let dir = temp_dir("schedules-next-run");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let state = test_state(&dir, &tasks_dir).await;

    // Seed directly in the DB: the list handler (not the scheduler) computes
    // `next_run`, and disabled rows must stay `null`.
    for (id, enabled) in [("enabled", true), ("disabled", false)] {
        crate::db::upsert_schedule(
            &state.db,
            &favetto_core::model::Schedule {
                id: id.to_string(),
                cron: "0 0 8 * * *".to_string(),
                task: "t".to_string(),
                input: serde_json::json!({}),
                enabled,
                last_run: None,
                next_run: None,
            },
        )
        .await
        .unwrap();
    }

    let resp = dispatch(
        &state,
        Request {
            id: 1,
            method: method::SCHEDULES_LIST.to_string(),
            params: serde_json::json!({}),
        },
    )
    .await;
    let result = resp.result.expect("schedules.list result");
    let list = result.as_array().expect("schedule array");
    let find = |id: &str| list.iter().find(|s| s["id"] == id).unwrap();
    assert!(
        find("enabled")["next_run"].is_string(),
        "enabled schedule should expose a next run: {list:?}"
    );
    assert!(
        find("disabled")["next_run"].is_null(),
        "disabled schedule should expose no next run: {list:?}"
    );

    // `schedules.upsert` echoes the same computed value for a new enabled row.
    let resp = dispatch(
        &state,
        Request {
            id: 2,
            method: method::SCHEDULES_UPSERT.to_string(),
            params: serde_json::json!({
                "id": "fresh",
                "cron": "0 0 8 * * *",
                "task": "t",
                "enabled": true,
            }),
        },
    )
    .await;
    let upserted = resp.result.expect("schedules.upsert result");
    assert!(
        upserted["next_run"].is_string(),
        "upsert should echo a next run: {upserted:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn catalog_list_exposes_the_task_kind() {
    let dir = temp_dir("catalog-kind");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(
        tasks_dir.join("agent_task.md"),
        "agent = \"x\"\n---\nbody\n",
    )
    .unwrap();
    std::fs::write(
        tasks_dir.join("action_task.md"),
        "kind = \"action\"\n\n[action]\nid = \"http.request\"\n---\nbody\n",
    )
    .unwrap();
    let state = test_state(&dir, &tasks_dir).await;

    let catalog = call_ok(&state, method::CATALOG_LIST, serde_json::json!({})).await;
    let entries = catalog.as_array().unwrap();
    let kind_of = |name: &str| {
        entries
            .iter()
            .find(|e| e["name"] == name)
            .and_then(|e| e["kind"].as_str())
            .unwrap_or("<missing>")
    };
    assert_eq!(kind_of("agent_task"), "agent");
    assert_eq!(kind_of("action_task"), "action");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn update_catalog_task_rewrites_file_and_publishes() {
    let dir = temp_dir("update");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(tasks_dir.join("a.md"), "agent = \"x\"\n---\noriginal\n").unwrap();
    let state = test_state(&dir, &tasks_dir).await;
    let mut rx = state.bus.subscribe();

    let updated = "agent = \"x\"\n---\nupdated body\n";
    let params = serde_json::json!({ "name": "a", "markdown": updated });
    update_catalog_task(&state, json_params(params.clone()))
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(tasks_dir.join("a.md")).unwrap(),
        updated
    );
    let prompt = state
        .catalog
        .read()
        .iter()
        .find(|d| d.name == "a")
        .map(|d| d.prompt.clone())
        .unwrap();
    assert_eq!(prompt, "updated body");
    assert!(matches!(rx.try_recv(), Ok(ServerPush::CatalogUpdated)));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn update_catalog_task_nested_path() {
    let dir = temp_dir("update-nested");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(tasks_dir.join("pipelines")).unwrap();
    std::fs::write(
        tasks_dir.join("pipelines/plan.md"),
        "agent = \"x\"\n---\none\n",
    )
    .unwrap();
    let state = test_state(&dir, &tasks_dir).await;

    let updated = "agent = \"x\"\n---\ntwo\n";
    let params = serde_json::json!({ "name": "pipelines/plan", "markdown": updated });
    update_catalog_task(&state, json_params(params.clone()))
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(tasks_dir.join("pipelines/plan.md")).unwrap(),
        updated
    );
    assert!(state
        .catalog
        .read()
        .iter()
        .any(|d| d.name == "pipelines/plan" && d.prompt == "two"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn update_catalog_task_rejects_unknown_name() {
    let dir = temp_dir("update-unknown");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let state = test_state(&dir, &tasks_dir).await;

    let params = serde_json::json!({
        "name": "ghost",
        "markdown": "agent = \"x\"\n---\nhi\n",
    });
    let err = update_catalog_task(&state, json_params(params.clone()))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unknown task"), "{err}");
    assert!(!tasks_dir.join("ghost.md").exists());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn update_catalog_task_rejects_invalid_markdown() {
    let dir = temp_dir("update-invalid");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let original = "agent = \"x\"\n---\noriginal\n";
    std::fs::write(tasks_dir.join("a.md"), original).unwrap();
    let state = test_state(&dir, &tasks_dir).await;

    let params = serde_json::json!({ "name": "a", "markdown": "agent = \n---\nbroken\n" });
    let err = update_catalog_task(&state, json_params(params.clone()))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("invalid task"), "{err}");
    assert_eq!(
        std::fs::read_to_string(tasks_dir.join("a.md")).unwrap(),
        original
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn update_catalog_task_rejects_traversal() {
    let dir = temp_dir("update-traversal");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let state = test_state(&dir, &tasks_dir).await;

    let params = serde_json::json!({
        "name": "../escape",
        "markdown": "agent = \"x\"\n---\nhi\n",
    });
    let err = update_catalog_task(&state, json_params(params.clone()))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("invalid task path"), "{err}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn dispatch_catalog_update_returns_updated() {
    let dir = temp_dir("update-dispatch");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(tasks_dir.join("a.md"), "agent = \"x\"\n---\noriginal\n").unwrap();
    let state = test_state(&dir, &tasks_dir).await;

    let req = Request {
        id: 1,
        method: method::CATALOG_UPDATE.to_string(),
        params: serde_json::json!({
            "name": "a",
            "markdown": "agent = \"x\"\n---\nnew\n",
        }),
    };
    let resp = dispatch(&state, req).await;
    assert!(resp.error.is_none(), "{:?}", resp.error);
    assert_eq!(
        resp.result.and_then(|v| v.get("updated").cloned()),
        Some(serde_json::json!(true))
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A fake `opencode session list --format json` executable that ignores its
/// arguments and prints `json`.
#[cfg(unix)]
/// A fake opencode that appends its argv to `capture` and exits 0.
#[cfg(unix)]
fn argv_capture_script(dir: &Path, capture: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("fake-opencode.sh");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\n",
            capture.display()
        ),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

/// A `State` built from an explicit config, with its registry resolved from it.
#[cfg(unix)]
async fn agent_state_with_config(
    dir: &Path,
    tasks_dir: &Path,
    cfg: crate::config::FavettoConfig,
) -> Arc<State> {
    let pool = crate::db::open(&dir.join("test.db")).await.unwrap();
    crate::db::migrate(&pool).await.unwrap();
    let registry = crate::agents::AgentRegistry::from_config_with(&cfg, &|_| true).unwrap();
    let catalog = Arc::new(parking_lot::RwLock::new(
        crate::tasks::load_catalog(tasks_dir).unwrap(),
    ));
    let scheduler = crate::scheduler::Scheduler::new().await.unwrap();
    Arc::new(State::new(StateInit {
        db: pool,
        bus: crate::event_bus::EventBus::new(64),
        token: favetto_core::auth::Token::generate(),
        webhooks: crate::webhooks::WebhookSecrets::from_config(&cfg),
        agents: crate::agents::AgentManager::new(),
        registry,
        config: Arc::new(cfg),
        config_path: dir.join("config.toml"),
        data_dir: dir.to_path_buf(),
        tasks_dir: tasks_dir.to_path_buf(),
        catalog,
        scheduler,
        hook_store: Arc::new(parking_lot::RwLock::new(Vec::new())),
    }))
}

/// A `State` whose registry carries the given `[agents.*]` entries and whose
/// `[agent].default` is `default`.
#[cfg(unix)]
async fn agent_state(
    dir: &Path,
    tasks_dir: &Path,
    default: &str,
    agents: Vec<(&str, crate::config::AgentConfig)>,
) -> Arc<State> {
    let mut cfg = crate::config::FavettoConfig::default();
    cfg.agent.default = Some(default.to_string());
    for (name, agent) in agents {
        cfg.agents.insert(name.to_string(), agent);
    }
    agent_state_with_config(dir, tasks_dir, cfg).await
}

/// A `State` whose `opencode` command is `command` (a title-lookup fixture).
#[cfg(unix)]
async fn title_state(dir: &Path, tasks_dir: &Path, command: &Path) -> Arc<State> {
    agent_state(
        dir,
        tasks_dir,
        "opencode",
        vec![(
            "opencode",
            crate::config::AgentConfig {
                command: command.to_string_lossy().into_owned(),
                ..Default::default()
            },
        )],
    )
    .await
}

fn oneshot_task() -> Task {
    Task {
        id: Uuid::new_v4(),
        name: "one-shot".to_string(),
        status: TaskStatus::Running,
        attempt: 1,
        input: serde_json::json!({ "oneshot": true }),
        output: None,
        dedupe_key: None,
        created_at: Timestamp::now(),
        started_at: Some(Timestamp::now()),
        finished_at: None,
        error: None,
        failure: None,
        session_id: None,
        session_title: None,
        parent_id: None,
        root_id: None,
        interactive: false,
        agent: None,
        provider: None,
        model: None,
        latest_usage: None,
    }
}

fn live_session(running: bool, headless: bool, awaiting: bool) -> AgentSessionInfo {
    AgentSessionInfo {
        id: "s1".to_string(),
        agent: "opencode".to_string(),
        backend: favetto_core::model::AgentBackend::Acp,
        task_id: None,
        running,
        headless,
        session_id: None,
        awaiting_input: awaiting.then(|| favetto_core::model::AwaitingInputReason {
            kind: favetto_core::model::AwaitingInputKind::Permission,
            message: "Allow?".to_string(),
            request_id: None,
            options: Vec::new(),
            allow_always: false,
            form: None,
        }),
        activity: None,
        usage: None,
        load_state: Default::default(),
        load_error: None,
        config_options: Vec::new(),
        modes: None,
        auth: None,
        mcp: None,
    }
}

#[test]
fn should_attach_to_live_attaches_every_running_session() {
    // Live interactive TUI: attach.
    assert!(should_attach_to_live(&live_session(true, false, false)));
    // A live headless ACP run has an in-process feed, so attach instead of
    // seeding a duplicate; the client decides whether it is writable.
    assert!(should_attach_to_live(&live_session(true, true, false)));
    // Headless but blocked on the user: attach so keystrokes reach it.
    assert!(should_attach_to_live(&live_session(true, true, true)));
    // Exited sessions are never attached.
    assert!(!should_attach_to_live(&live_session(false, true, true)));
    assert!(!should_attach_to_live(&live_session(false, false, false)));
}

/// Build a real-command agent entry so history loadability does not depend on
/// the host's PATH.
fn available_agent_config() -> crate::config::FavettoConfig {
    let mut config = crate::config::FavettoConfig::default();
    config.agents.insert(
        "opencode".to_string(),
        crate::config::AgentConfig {
            command: "/bin/true".to_string(),
            ..Default::default()
        },
    );
    config
}

#[tokio::test]
async fn tasks_session_resolves_the_most_recent_record_for_the_task() {
    let dir = temp_dir("tasks-session");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let state = test_state_with_config(&dir, &tasks_dir, available_agent_config()).await;
    let now = Timestamp::now();
    let task_id = Uuid::new_v4();
    let task_id_text = task_id.to_string();

    let mut older =
        crate::db::AgentSessionRecord::loading("local-older", "opencode", dir.join("work"), now);
    older.task_id = Some(task_id_text.clone());
    older.external_session_id = Some("provider-acp-secret".to_string());
    older.status = crate::db::AgentSessionStatus::Ended;
    crate::db::insert_agent_session(&state.db, &older)
        .await
        .unwrap();

    let mut newer = crate::db::AgentSessionRecord::loading(
        "local-newer",
        "opencode",
        dir.join("work"),
        now + time::Duration::seconds(1),
    );
    newer.task_id = Some(task_id_text.clone());
    newer.external_session_id = Some("provider-acp-newer".to_string());
    newer.status = crate::db::AgentSessionStatus::Ended;
    crate::db::insert_agent_session(&state.db, &newer)
        .await
        .unwrap();

    let mut unrelated = crate::db::AgentSessionRecord::loading(
        "local-unrelated",
        "opencode",
        dir.join("work"),
        now + time::Duration::seconds(2),
    );
    unrelated.task_id = Some(Uuid::new_v4().to_string());
    crate::db::insert_agent_session(&state.db, &unrelated)
        .await
        .unwrap();

    let resp = dispatch(
        &state,
        Request {
            id: 1,
            method: method::TASKS_SESSION.to_string(),
            params: serde_json::json!({ "id": task_id }),
        },
    )
    .await;
    let result = resp.result.expect("tasks.session result");
    assert_eq!(result["session"]["id"], "local-newer");
    assert_eq!(result["session"]["task_id"], task_id_text);
    assert_eq!(result["session"]["state"], "ended");
    assert_eq!(result["session"]["loadable"], true);
    assert!(
        !result.to_string().contains("provider-acp"),
        "tasks.session must not expose the provider ACP id"
    );

    let empty = dispatch(
        &state,
        Request {
            id: 2,
            method: method::TASKS_SESSION.to_string(),
            params: serde_json::json!({ "id": Uuid::new_v4() }),
        },
    )
    .await;
    let empty = empty.result.expect("empty tasks.session result");
    assert!(empty["session"].is_null(), "{empty}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn tasks_session_reuses_history_loadability_derivation() {
    let dir = temp_dir("tasks-session-unloadable");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let state = test_state_with_config(&dir, &tasks_dir, available_agent_config()).await;
    let task_id = Uuid::new_v4();

    let mut unlinked = crate::db::AgentSessionRecord::loading(
        "local-no-id",
        "opencode",
        dir.join("work"),
        Timestamp::now(),
    );
    unlinked.task_id = Some(task_id.to_string());
    unlinked.status = crate::db::AgentSessionStatus::Ended;
    crate::db::insert_agent_session(&state.db, &unlinked)
        .await
        .unwrap();

    let resp = dispatch(
        &state,
        Request {
            id: 1,
            method: method::TASKS_SESSION.to_string(),
            params: serde_json::json!({ "id": task_id }),
        },
    )
    .await;
    let result = resp.result.expect("tasks.session result");
    assert_eq!(result["session"]["id"], "local-no-id");
    assert_eq!(result["session"]["loadable"], false);
    assert!(
        result["session"]["load_error"]
            .as_str()
            .is_some_and(|message| message.contains("no ACP session id")),
        "{result}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn agents_history_lists_persisted_metadata_and_loadability() {
    let dir = temp_dir("agents-history");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let state = test_state_with_config(&dir, &tasks_dir, available_agent_config()).await;
    let now = Timestamp::now();

    let mut ready =
        crate::db::AgentSessionRecord::loading("local-ready", "opencode", dir.join("work"), now);
    ready.external_session_id = Some("provider-acp-secret".to_string());
    ready.status = crate::db::AgentSessionStatus::Ready;
    ready.title = Some("Fix the widget".to_string());
    crate::db::insert_agent_session(&state.db, &ready)
        .await
        .unwrap();

    let mut failed =
        crate::db::AgentSessionRecord::loading("local-failed", "opencode", dir.join("work"), now);
    failed.status = crate::db::AgentSessionStatus::Failed;
    failed.error = Some("agent could not load ACP session".to_string());
    crate::db::insert_agent_session(&state.db, &failed)
        .await
        .unwrap();

    let mut unlinked =
        crate::db::AgentSessionRecord::loading("local-no-id", "opencode", dir.join("work"), now);
    unlinked.status = crate::db::AgentSessionStatus::Ended;
    crate::db::insert_agent_session(&state.db, &unlinked)
        .await
        .unwrap();

    let resp = dispatch(
        &state,
        Request {
            id: 1,
            method: method::AGENTS_HISTORY.to_string(),
            params: serde_json::json!({ "limit": 10 }),
        },
    )
    .await;
    let history = resp.result.expect("agents.history result");
    let rows = history.as_array().expect("history array");
    assert_eq!(rows.len(), 3);

    // The provider ACP id is never a client routing key and never leaves the
    // daemon in the history payload.
    assert!(
        !history.to_string().contains("provider-acp-secret"),
        "history must not expose the provider ACP id"
    );

    let by_id = |id: &str| {
        rows.iter()
            .find(|row| row["id"] == serde_json::json!(id))
            .unwrap_or_else(|| panic!("missing history row {id}"))
    };
    let ready = by_id("local-ready");
    assert_eq!(ready["agent"], "opencode");
    assert_eq!(ready["title"], "Fix the widget");
    assert_eq!(ready["cwd"], serde_json::json!(dir.join("work")));
    assert_eq!(ready["state"], "ended");
    assert_eq!(ready["loadable"], true);
    assert!(ready.get("load_error").is_none());

    let failed = by_id("local-failed");
    assert_eq!(failed["state"], "failed");
    assert_eq!(failed["loadable"], false);
    assert!(
        failed["load_error"]
            .as_str()
            .is_some_and(|message| message.contains("could not load")),
        "a failed restore keeps its reason: {failed:?}"
    );

    let unlinked = by_id("local-no-id");
    assert_eq!(unlinked["state"], "ended");
    assert_eq!(unlinked["loadable"], false);
    assert!(unlinked["load_error"]
        .as_str()
        .is_some_and(|message| message.contains("no ACP session id")));

    // `agents.list` keeps its existing shape and live-session semantics: a
    // persisted-only record does not masquerade as a live session there.
    let list = dispatch(
        &state,
        Request {
            id: 2,
            method: method::AGENTS_LIST.to_string(),
            params: serde_json::json!({}),
        },
    )
    .await;
    let agents = list.result.expect("agents.list result");
    assert!(
        agents
            .as_array()
            .expect("agent catalog")
            .iter()
            .all(|agent| agent["sessions"].as_array().is_some_and(Vec::is_empty)),
        "persisted history must not be folded into agents.list: {agents}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn agents_load_routes_only_by_favetto_local_id() {
    let dir = temp_dir("agents-load-routing");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let state = test_state_with_config(&dir, &tasks_dir, available_agent_config()).await;
    let now = Timestamp::now();

    let mut ready =
        crate::db::AgentSessionRecord::loading("local-ready", "opencode", dir.join("work"), now);
    ready.external_session_id = Some("provider-acp-secret".to_string());
    ready.status = crate::db::AgentSessionStatus::Ready;
    crate::db::insert_agent_session(&state.db, &ready)
        .await
        .unwrap();

    // A provider ACP id is not a local record: `agents.load` must refuse it
    // rather than guess or start a fresh session.
    let error = load_agent(&state, "provider-acp-secret", None)
        .await
        .expect_err("provider ids are not local routing keys");
    assert!(
        error.to_string().contains("no persisted agent session"),
        "{error}"
    );

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(4);
    let subscribed: Attached = Arc::new(Mutex::new(HashSet::new()));
    let live_task: LiveTask = Arc::new(Mutex::new(None));
    handle_request(
        &state,
        &tx,
        &subscribed,
        &live_task,
        Request {
            id: 9,
            method: method::AGENTS_LOAD.to_string(),
            params: serde_json::json!({ "session_id": "missing-local" }),
        },
    )
    .await;
    let frame = rx.recv().await.expect("a response frame");
    let error = match frame {
        Frame::Response(resp) => resp.error.expect("unknown id must error"),
        other => panic!("expected a response, got {other:?}"),
    };
    assert_eq!(error.code, error_code::INVALID_PARAMS);
    assert!(
        error.message.contains("no persisted agent session"),
        "{error:?}"
    );
    assert!(subscribed.lock().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
/// The panel session must inherit the catalog task's `provider`/`model`, so
/// an interactive reattach uses the same route as the headless run.
/// Entering an action task must not spawn an agent: `agents.start` rejects it
/// server-side instead of falling back to `[agent].default`. Regression for
/// #324.
#[cfg(unix)]
#[tokio::test]
async fn agents_start_rejects_an_action_task() {
    let dir = temp_dir("start-action-reject");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(
        tasks_dir.join("gate.md"),
        "kind = \"action\"\nworktree = false\n\n[action]\nid = \"http.request\"\n---\nPoll.\n",
    )
    .unwrap();

    // A fake opencode that records the argv it was launched with: the guard must
    // trip before any PTY is spawned, so this stays empty.
    let capture = dir.join("captured-args.txt");
    let script = argv_capture_script(&dir, &capture);
    let state = title_state(&dir, &tasks_dir, &script).await;

    let task = Task {
        id: Uuid::new_v4(),
        name: "gate".to_string(),
        status: TaskStatus::Succeeded,
        attempt: 1,
        input: serde_json::json!({}),
        output: Some(serde_json::json!({
            "envelope": { "summary": "http 200: 3 issues" }
        })),
        dedupe_key: None,
        created_at: Timestamp::now(),
        started_at: Some(Timestamp::now()),
        finished_at: Some(Timestamp::now()),
        error: None,
        failure: None,
        session_id: None,
        session_title: None,
        parent_id: None,
        root_id: None,
        interactive: false,
        agent: None,
        provider: None,
        model: None,
        latest_usage: None,
    };
    db::insert_task(&state.db, &task).await.unwrap();

    let err = start_agent(
        &state,
        json_params(serde_json::json!({ "task_id": task.id.to_string() })),
    )
    .await
    .expect_err("an action task must not open an agent session");
    assert!(
        err.to_string().contains("action task"),
        "expected a clear action-task error, got: {err}"
    );
    assert!(
        std::fs::read_to_string(&capture)
            .unwrap_or_default()
            .is_empty(),
        "no PTY may be spawned for an action task"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// End-to-end regression for issue #69: starting a catalog task through
/// `tasks.start` alone runs it headlessly and submits the rendered prompt —
/// no `agents.start` / Agent-panel attach is needed.
/// `tasks.start` is headless by default; only the TUI opts a run into the
/// interactive TUI by sending `interactive: true`.
#[tokio::test]
async fn tasks_start_marks_the_task_interactive_only_when_requested() {
    let dir = temp_dir("tasks-start-interactive");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(tasks_dir.join("issue.md"), "agent = \"x\"\n---\nbody\n").unwrap();
    let state = test_state(&dir, &tasks_dir).await;

    let start = |id: u64, interactive: Option<bool>| {
        let state = state.clone();
        async move {
            let mut params = serde_json::json!({ "name": "issue" });
            if let Some(interactive) = interactive {
                params["interactive"] = serde_json::json!(interactive);
            }
            let resp = dispatch(
                &state,
                Request {
                    id,
                    method: method::TASKS_START.to_string(),
                    params,
                },
            )
            .await;
            assert!(resp.error.is_none(), "{:?}", resp.error);
            let result = resp.result.expect("tasks.start result");
            Uuid::parse_str(result["id"].as_str().expect("task id")).expect("uuid")
        }
    };

    // The TUI's request marks the run interactive.
    let user_id = start(1, Some(true)).await;
    assert!(
        db::get_task(&state.db, user_id)
            .await
            .unwrap()
            .unwrap()
            .interactive
    );

    // A plain `tasks.start` (raw RPC / automation) stays headless.
    let programmatic_id = start(2, None).await;
    assert!(
        !db::get_task(&state.db, programmatic_id)
            .await
            .unwrap()
            .unwrap()
            .interactive
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn tasks_start_persists_agent_and_model_overrides_across_retry() {
    let dir = temp_dir("tasks-start-overrides");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(
        tasks_dir.join("issue.md"),
        "agent = \"x\"\nprovider = \"p\"\nmodel = \"m\"\n---\nbody\n",
    )
    .unwrap();
    let state = test_state(&dir, &tasks_dir).await;

    let started = call_ok(
        &state,
        method::TASKS_START,
        serde_json::json!({
            "name": "issue",
            "agent": "y",
            "provider": "q",
            "model": "z",
        }),
    )
    .await;
    assert_eq!(started["agent"], "y");
    assert_eq!(started["provider"], "q");
    assert_eq!(started["model"], "z");

    let id = Uuid::parse_str(started["id"].as_str().expect("task id")).expect("uuid");
    let stored = db::get_task(&state.db, id).await.unwrap().unwrap();
    assert_eq!(stored.agent.as_deref(), Some("y"));
    assert_eq!(stored.provider.as_deref(), Some("q"));
    assert_eq!(stored.model.as_deref(), Some("z"));

    // Retry re-enqueues the same row; the launch overrides must survive.
    let mut terminal = stored.clone();
    terminal.status = TaskStatus::Failed;
    terminal.error = Some("boom".to_string());
    terminal.failure = Some(favetto_core::model::Failure::new(
        favetto_core::model::FailureKind::Agent,
        "boom",
    ));
    db::upsert_task(&state.db, &terminal).await.unwrap();
    let retried = call_ok(&state, method::TASKS_RETRY, serde_json::json!({ "id": id })).await;
    assert_eq!(retried["agent"], "y");
    assert_eq!(retried["provider"], "q");
    assert_eq!(retried["model"], "z");

    // Blank overrides are normalized away rather than stored as empty strings.
    let blank = call_ok(
        &state,
        method::TASKS_START,
        serde_json::json!({ "name": "issue", "agent": "  ", "model": "" }),
    )
    .await;
    assert!(blank.get("agent").is_none(), "{blank}");
    assert!(blank.get("model").is_none(), "{blank}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn tasks_start_rejects_agent_overrides_for_action_tasks() {
    let dir = temp_dir("tasks-start-action-overrides");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(
        tasks_dir.join("gate.md"),
        "kind = \"action\"\n[action]\nid = \"http.request\"\n---\nbody\n",
    )
    .unwrap();
    let state = test_state(&dir, &tasks_dir).await;

    let resp = dispatch(
        &state,
        Request {
            id: 1,
            method: method::TASKS_START.to_string(),
            params: serde_json::json!({ "name": "gate", "agent": "y" }),
        },
    )
    .await;
    let error = resp.error.expect("action override rejected");
    assert_eq!(error.code, favetto_core::rpc::error_code::INVALID_PARAMS);
    assert!(error.message.contains("action task"), "{error:?}");

    // Without overrides the same action task starts normally.
    let ok = call_ok(
        &state,
        method::TASKS_START,
        serde_json::json!({ "name": "gate" }),
    )
    .await;
    assert_eq!(ok["name"], "gate");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn tasks_list_omits_output_and_honours_limit_and_get_returns_it() {
    let dir = temp_dir("tasks-list-get");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let state = test_state(&dir, &tasks_dir).await;

    let make = |name: &str| Task {
        id: Uuid::new_v4(),
        name: name.to_string(),
        status: TaskStatus::Succeeded,
        attempt: 0,
        input: serde_json::json!({}),
        output: Some(serde_json::json!({ "output": "x".repeat(20_000) })),
        dedupe_key: None,
        created_at: Timestamp::now(),
        started_at: None,
        finished_at: Some(Timestamp::now()),
        error: None,
        failure: None,
        session_id: None,
        session_title: None,
        parent_id: None,
        root_id: None,
        interactive: false,
        agent: None,
        provider: None,
        model: None,
        latest_usage: None,
    };
    let first = make("one");
    let second = make("two");
    db::upsert_task(&state.db, &first).await.unwrap();
    db::upsert_task(&state.db, &second).await.unwrap();

    // `tasks.list` carries metadata only.
    let resp = dispatch(
        &state,
        Request {
            id: 1,
            method: method::TASKS_LIST.to_string(),
            params: serde_json::json!({}),
        },
    )
    .await;
    let list = resp.result.expect("tasks.list result");
    let list = list.as_array().expect("array");
    assert_eq!(list.len(), 2);
    assert!(
        list.iter().all(|t| t.get("output").is_none()),
        "list must omit output: {list:?}"
    );

    // `limit` narrows the list.
    let resp = dispatch(
        &state,
        Request {
            id: 2,
            method: method::TASKS_LIST.to_string(),
            params: serde_json::json!({ "limit": 1 }),
        },
    )
    .await;
    assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 1);

    // `tasks.get` returns the full output blob.
    let resp = dispatch(
        &state,
        Request {
            id: 3,
            method: method::TASKS_GET.to_string(),
            params: serde_json::json!({ "id": first.id }),
        },
    )
    .await;
    assert!(resp.result.unwrap().get("output").is_some());

    // Unknown ids are an invalid-params error.
    let resp = dispatch(
        &state,
        Request {
            id: 4,
            method: method::TASKS_GET.to_string(),
            params: serde_json::json!({ "id": Uuid::new_v4() }),
        },
    )
    .await;
    assert_eq!(resp.error.unwrap().code, error_code::INVALID_PARAMS);

    let _ = std::fs::remove_dir_all(&dir);
}

/// `tasks.runs` returns a task's execution attempts oldest-first, and an unknown
/// id yields an empty array rather than an error.
#[tokio::test]
async fn tasks_runs_returns_history_oldest_first() {
    use favetto_core::model::{RunStatus, TaskRun};

    let dir = temp_dir("tasks-runs");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let state = test_state(&dir, &tasks_dir).await;

    let task = Task {
        id: Uuid::new_v4(),
        name: "hist".to_string(),
        status: TaskStatus::Failed,
        attempt: 3,
        input: serde_json::json!({}),
        output: None,
        dedupe_key: None,
        created_at: Timestamp::now(),
        started_at: Some(Timestamp::now()),
        finished_at: Some(Timestamp::now()),
        error: None,
        failure: None,
        session_id: None,
        session_title: None,
        parent_id: None,
        root_id: None,
        interactive: false,
        agent: None,
        provider: None,
        model: None,
        latest_usage: None,
    };
    db::upsert_task(&state.db, &task).await.unwrap();

    let make_run = |attempt: u32, status: RunStatus| TaskRun {
        id: Uuid::new_v4(),
        task_id: task.id,
        attempt,
        status,
        agent: Some("opencode".to_string()),
        model: None,
        session_id: None,
        started_at: Some(Timestamp::now()),
        finished_at: Some(Timestamp::now()),
        exit_code: None,
        error: None,
        usage: None,
        failure: None,
    };
    // Insert out of order to prove the method returns attempt order.
    db::insert_task_run(&state.db, &make_run(2, RunStatus::Failed))
        .await
        .unwrap();
    db::insert_task_run(&state.db, &make_run(1, RunStatus::Succeeded))
        .await
        .unwrap();
    db::insert_task_run(&state.db, &make_run(3, RunStatus::Running))
        .await
        .unwrap();

    let resp = dispatch(
        &state,
        Request {
            id: 1,
            method: method::TASKS_RUNS.to_string(),
            params: serde_json::json!({ "id": task.id }),
        },
    )
    .await;
    let result = resp.result.expect("tasks.runs result");
    let runs: Vec<TaskRun> = serde_json::from_value(result).expect("decodes to Vec<TaskRun>");
    assert_eq!(
        runs.iter().map(|r| r.attempt).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "runs must come back oldest-first"
    );
    assert_eq!(runs[1].status, RunStatus::Failed);
    assert_eq!(runs[0].agent.as_deref(), Some("opencode"));

    // An unknown id is empty history, not an error.
    let resp = dispatch(
        &state,
        Request {
            id: 2,
            method: method::TASKS_RUNS.to_string(),
            params: serde_json::json!({ "id": Uuid::new_v4() }),
        },
    )
    .await;
    assert!(resp.error.is_none());
    assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 0);

    let _ = std::fs::remove_dir_all(&dir);
}

/// `tasks.retry` re-enqueues a terminal task and keeps its run history, but is
/// rejected while a run is still live.
#[tokio::test]
async fn tasks_retry_requeues_terminal_tasks_and_rejects_live_runs() {
    use favetto_core::model::{Failure, FailureKind, RunStatus, TaskRun};

    let dir = temp_dir("tasks-retry");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let state = test_state(&dir, &tasks_dir).await;

    let make = |name: &str, status: TaskStatus| Task {
        id: Uuid::new_v4(),
        name: name.to_string(),
        status,
        attempt: 1,
        input: serde_json::json!({}),
        output: None,
        dedupe_key: None,
        created_at: Timestamp::now(),
        started_at: Some(Timestamp::now()),
        finished_at: Some(Timestamp::now()),
        error: None,
        failure: None,
        session_id: None,
        session_title: None,
        parent_id: None,
        root_id: None,
        interactive: false,
        agent: None,
        provider: None,
        model: None,
        latest_usage: None,
    };
    let run = |task_id: Uuid, attempt: u32, status: RunStatus| TaskRun {
        id: Uuid::new_v4(),
        task_id,
        attempt,
        status,
        agent: None,
        model: None,
        session_id: None,
        started_at: Some(Timestamp::now()),
        finished_at: None,
        exit_code: None,
        error: None,
        usage: None,
        failure: None,
    };

    // A failed task with run history can be retried; the history is kept.
    let mut failed = make("failed", TaskStatus::Failed);
    failed.error = Some("boom".to_string());
    failed.failure = Some(Failure::new(FailureKind::Agent, "boom"));
    db::upsert_task(&state.db, &failed).await.unwrap();
    db::insert_task_run(&state.db, &run(failed.id, 1, RunStatus::Failed))
        .await
        .unwrap();

    let resp = dispatch(
        &state,
        Request {
            id: 1,
            method: method::TASKS_RETRY.to_string(),
            params: serde_json::json!({ "id": failed.id }),
        },
    )
    .await;
    let result = resp.result.expect("retry result");
    assert_eq!(result["status"], "pending");
    assert_eq!(result["attempt"], 1);
    assert_eq!(
        db::list_task_runs(&state.db, failed.id)
            .await
            .unwrap()
            .len(),
        1,
        "manual retry preserves run history"
    );

    // A live run is rejected.
    let running = make("running", TaskStatus::Running);
    db::upsert_task(&state.db, &running).await.unwrap();
    db::insert_task_run(&state.db, &run(running.id, 1, RunStatus::Running))
        .await
        .unwrap();
    let resp = dispatch(
        &state,
        Request {
            id: 2,
            method: method::TASKS_RETRY.to_string(),
            params: serde_json::json!({ "id": running.id }),
        },
    )
    .await;
    let err = resp.error.expect("live run rejected");
    assert_eq!(err.code, error_code::INVALID_PARAMS);
    assert!(err.message.contains("live run"), "{}", err.message);

    // A pending task is not terminal either.
    let pending = make("pending", TaskStatus::Pending);
    db::upsert_task(&state.db, &pending).await.unwrap();
    let resp = dispatch(
        &state,
        Request {
            id: 3,
            method: method::TASKS_RETRY.to_string(),
            params: serde_json::json!({ "id": pending.id }),
        },
    )
    .await;
    assert_eq!(resp.error.unwrap().code, error_code::INVALID_PARAMS);

    // An unknown id is invalid params, not an internal error.
    let resp = dispatch(
        &state,
        Request {
            id: 4,
            method: method::TASKS_RETRY.to_string(),
            params: serde_json::json!({ "id": Uuid::new_v4() }),
        },
    )
    .await;
    assert_eq!(resp.error.unwrap().code, error_code::INVALID_PARAMS);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Malformed params must fail with `INVALID_PARAMS` and a stable message for
/// every arm that parses them, rather than reaching the database or PTY.
/// Optional params keep their old lenient behaviour: an ill-typed value is
/// treated as absent so the default applies, preserving the wire contract.
/// Parameter validation surfaces a typed [`RpcError::InvalidParams`] rather
/// than a bare tuple, and it maps to the original wire code and message.
#[test]
fn parse_params_returns_a_typed_invalid_params_error() {
    let error = parse_params::<TaskIdParams>(method::TASKS_GET, &serde_json::json!({}))
        .expect_err("missing `id` must be rejected");
    assert!(matches!(error, RpcError::InvalidParams(_)), "{error:?}");
    assert_eq!(error.code(), error_code::INVALID_PARAMS);
    assert!(
        error.message().starts_with("invalid params for tasks.get:"),
        "{}",
        error.message()
    );
    assert_eq!(error.to_object().code, error_code::INVALID_PARAMS);
}

/// A depth-bounded arbitrary JSON value, mirroring the strategy in
/// `favetto_core::wire`'s round-trip tests. Only finite numbers are generated so
/// value construction itself can never panic.
fn arb_hostile_json(depth: u32) -> BoxedStrategy<serde_json::Value> {
    if depth == 0 {
        prop_oneof![
            Just(serde_json::Value::Null),
            any::<bool>().prop_map(serde_json::Value::Bool),
            any::<i64>().prop_map(|n| serde_json::Value::Number(n.into())),
            "[ -~]{0,24}".prop_map(serde_json::Value::String),
        ]
        .boxed()
    } else {
        prop_oneof![
            arb_hostile_json(0),
            prop::collection::btree_map("[a-z_]{1,8}", arb_hostile_json(depth - 1), 0..4)
                .prop_map(|m| serde_json::Value::Object(m.into_iter().collect())),
            prop::collection::vec(arb_hostile_json(depth - 1), 0..4)
                .prop_map(serde_json::Value::Array),
        ]
        .boxed()
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// `parse_params` is the single deserialization boundary for every RPC arm.
    /// No arbitrary JSON value may make it panic; a panic here would fail the
    /// request loop and drop every attached TUI session.
    #[test]
    fn parse_params_never_panics_on_hostile_values(value in arb_hostile_json(3)) {
        // Pure deserialization only: no side-effecting handler is invoked.
        let _: Result<TaskIdParams, _> = parse_params(method::TASKS_RETRY, &value);
        let _: Result<WorkflowInspectParams, _> = parse_params(method::WORKFLOW_INSPECT, &value);
        let _: Result<WorkflowRetryParams, _> = parse_params(method::WORKFLOW_RETRY, &value);
        let _: Result<WorkflowCreateParams, _> = parse_params(method::WORKFLOW_CREATE, &value);
        let _: Result<WorkflowSpawnParams, _> = parse_params(method::WORKFLOW_SPAWN, &value);
        let _: Result<StartAgentParams, _> = parse_params(method::AGENTS_START, &value);
        let _: Result<StartOneshotParams, _> = parse_params(method::TASKS_START_ONESHOT, &value);
        let _: Result<CatalogAddParams, _> = parse_params(method::CATALOG_ADD, &value);
        let _: Result<CatalogUpdateParams, _> = parse_params(method::CATALOG_UPDATE, &value);
        let _: Result<ScheduleUpsertParams, _> = parse_params(method::SCHEDULES_UPSERT, &value);
        let _: Result<HookUpsertParams, _> = parse_params(method::HOOKS_UPSERT, &value);
        let _: Result<AgentReplyParams, _> = parse_params(method::AGENTS_REPLY, &value);
        let _: Result<AgentElicitationReplyParams, _> =
            parse_params(method::AGENTS_REPLY_ELICITATION, &value);
        let _: Result<NotificationTestParams, _> = parse_params(method::NOTIFICATIONS_TEST, &value);
        let _: Result<SessionIdParams, _> = parse_params(method::AGENTS_ATTACH, &value);
    }
}

/// `agents.attach` / `agents.close` are special-cased in `handle_request` (they
/// subscribe/unsubscribe the connection), so they bypass `dispatch`. Malformed
/// session params must still be a typed `INVALID_PARAMS` before the session
/// manager is touched.
#[tokio::test]
async fn handle_request_rejects_malformed_special_params() {
    let dir = temp_dir("malformed-special");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let state = test_state(&dir, &tasks_dir).await;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(4);
    let subscribed: Attached = Arc::new(Mutex::new(HashSet::new()));
    let live_task: LiveTask = Arc::new(Mutex::new(None));

    for (id, method) in [
        (1u64, method::AGENTS_ATTACH),
        (2u64, method::AGENTS_CLOSE),
        (3u64, method::AGENTS_LOAD),
    ] {
        handle_request(
            &state,
            &tx,
            &subscribed,
            &live_task,
            Request {
                id,
                method: method.to_string(),
                params: serde_json::json!({ "session_id": 5 }),
            },
        )
        .await;
        let frame = rx.recv().await.expect("a response frame");
        let error = match frame {
            Frame::Response(resp) => resp.error.expect("malformed params must error"),
            other => panic!("expected a response, got {other:?}"),
        };
        assert_eq!(error.code, error_code::INVALID_PARAMS, "{method}");
        assert!(
            error.message.contains("invalid params"),
            "{method}: {error:?}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// `agents.reply` deserializes its typed reply and routes it to the session's
/// `InputResponder`; an unknown session is an internal error.
/// `tasks.events` backfills the structured feed from the task's latest session
/// and reports `session_id: None` when the task has no session at all.
/// A state whose catalog contains one trivial task per name in `names`.
async fn dag_state(tag: &str, names: &[&str]) -> (std::path::PathBuf, Arc<State>) {
    let dir = temp_dir(tag);
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    for name in names {
        std::fs::write(
            tasks_dir.join(format!("{name}.md")),
            format!("agent = \"x\"\n---\n{name}\n"),
        )
        .unwrap();
    }
    let state = test_state(&dir, &tasks_dir).await;
    (dir, state)
}

/// Build `workflow.create` params, optionally extending an existing root.
fn create_params(
    idempotency_key: &str,
    root_id: Option<Uuid>,
    tasks: serde_json::Value,
) -> serde_json::Value {
    let mut params = serde_json::json!({
        "idempotency_key": idempotency_key,
        "tasks": tasks,
    });
    if let Some(root_id) = root_id {
        params["root_id"] = serde_json::json!(root_id);
    }
    params
}

async fn dispatch_create(state: &Arc<State>, id: u64, params: serde_json::Value) -> Response {
    dispatch(
        state,
        Request {
            id,
            method: method::WORKFLOW_CREATE.to_string(),
            params,
        },
    )
    .await
}

fn node_id(result: &serde_json::Value, key: &str) -> Uuid {
    Uuid::parse_str(
        result["tasks"]
            .as_array()
            .expect("tasks array")
            .iter()
            .find(|t| t["key"] == key)
            .unwrap_or_else(|| panic!("no node with key {key}"))["id"]
            .as_str()
            .expect("id string"),
    )
    .unwrap()
}

#[tokio::test]
async fn workflow_create_inserts_a_dag_and_returns_ids() {
    let (dir, state) = dag_state("create-dag", &["a", "b", "c"]).await;
    let params = create_params(
        "op1",
        None,
        serde_json::json!([
            { "key": "a", "name": "a" },
            { "key": "b", "name": "b", "depends_on": ["a"] },
            { "key": "c", "name": "c", "depends_on": ["b"] },
        ]),
    );

    let resp = dispatch_create(&state, 1, params).await;
    let result = resp.result.expect("create result");
    let root = Uuid::parse_str(result["root_id"].as_str().unwrap()).unwrap();
    assert_eq!(result["tasks"].as_array().unwrap().len(), 3);

    let id_a = node_id(&result, "a");
    let id_b = node_id(&result, "b");
    let id_c = node_id(&result, "c");
    assert_eq!(root, id_a, "the first task is the root");

    for (id, is_root) in [(id_a, true), (id_b, false), (id_c, false)] {
        let row = db::get_task(&state.db, id).await.unwrap().unwrap();
        assert_eq!(row.status, TaskStatus::Pending);
        assert_eq!(row.root_id, Some(root));
        assert_eq!(row.parent_id, if is_root { None } else { Some(root) });
    }

    assert_eq!(
        db::list_dependencies(&state.db, id_b).await.unwrap(),
        vec![id_a]
    );
    assert_eq!(
        db::list_dependencies(&state.db, id_c).await.unwrap(),
        vec![id_b]
    );

    // Only the root is ready to dispatch: b and c wait on their predecessors.
    let pending: Vec<Uuid> = db::next_pending_tasks(&state.db, 50)
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(pending, vec![id_a]);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn workflow_create_runs_in_dependency_order() {
    let (dir, state) = dag_state("create-order", &["a", "b"]).await;
    let resp = dispatch_create(
        &state,
        1,
        create_params(
            "op-order",
            None,
            serde_json::json!([
                { "key": "a", "name": "a" },
                { "key": "b", "name": "b", "depends_on": ["a"] },
            ]),
        ),
    )
    .await;
    let result = resp.result.expect("create result");
    let id_a = node_id(&result, "a");
    let id_b = node_id(&result, "b");

    // Readiness is re-derived from the DB — no in-memory dispatcher state.
    let mut a = db::get_task(&state.db, id_a).await.unwrap().unwrap();
    a.status = TaskStatus::Succeeded;
    a.finished_at = Some(Timestamp::now());
    db::upsert_task(&state.db, &a).await.unwrap();

    let pending: Vec<Uuid> = db::next_pending_tasks(&state.db, 50)
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(pending, vec![id_b]);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn workflow_create_dedupes_re_submission() {
    let (dir, state) = dag_state("create-dedupe", &["a", "b"]).await;
    let params = || {
        create_params(
            "op-dedupe",
            None,
            serde_json::json!([
                { "key": "a", "name": "a" },
                { "key": "b", "name": "b", "depends_on": ["a"] },
            ]),
        )
    };

    let first = dispatch_create(&state, 1, params()).await.result.unwrap();
    let second = dispatch_create(&state, 2, params()).await.result.unwrap();
    assert_eq!(first["root_id"], second["root_id"]);
    assert_eq!(node_id(&first, "b"), node_id(&second, "b"));

    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(rows, 2, "re-submission must not create a second run");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn workflow_create_rejects_invalid_requests() {
    let (dir, state) = dag_state("create-invalid", &["a", "b"]).await;
    let assert_invalid = |resp: Response, needle: &str| {
        assert!(resp.result.is_none(), "expected an error");
        let error = resp.error.expect("error object");
        assert_eq!(error.code, error_code::INVALID_PARAMS);
        assert!(error.message.contains(needle), "{error:?}");
    };

    // Unknown catalog name.
    assert_invalid(
        dispatch_create(
            &state,
            1,
            create_params(
                "k1",
                None,
                serde_json::json!([{ "key": "x", "name": "ghost" }]),
            ),
        )
        .await,
        "unknown task",
    );
    // Empty task list.
    assert_invalid(
        dispatch_create(&state, 2, create_params("k2", None, serde_json::json!([]))).await,
        "at least one task",
    );
    // Duplicate key.
    assert_invalid(
        dispatch_create(
            &state,
            3,
            create_params(
                "k3",
                None,
                serde_json::json!([
                    { "key": "dup", "name": "a" },
                    { "key": "dup", "name": "b" },
                ]),
            ),
        )
        .await,
        "duplicate task key",
    );
    // Dependency on a key not in the request.
    assert_invalid(
        dispatch_create(
            &state,
            4,
            create_params(
                "k4",
                None,
                serde_json::json!([
                    { "key": "a", "name": "a", "depends_on": ["missing"] },
                ]),
            ),
        )
        .await,
        "unknown key",
    );
    // Cycle.
    assert_invalid(
        dispatch_create(
            &state,
            5,
            create_params(
                "k5",
                None,
                serde_json::json!([
                    { "key": "a", "name": "a", "depends_on": ["b"] },
                    { "key": "b", "name": "b", "depends_on": ["a"] },
                ]),
            ),
        )
        .await,
        "cycle",
    );
    // Unknown root.
    assert_invalid(
        dispatch_create(
            &state,
            6,
            create_params(
                "k6",
                Some(Uuid::new_v4()),
                serde_json::json!([{ "key": "a", "name": "a" }]),
            ),
        )
        .await,
        "root",
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn workflow_spawn_records_depends_on_and_root() {
    let (dir, state) = dag_state("spawn-dyn", &["child"]).await;
    let root = oneshot_task();
    db::upsert_task(&state.db, &root).await.unwrap();
    let mut predecessor = oneshot_task();
    predecessor.root_id = Some(root.id);
    predecessor.parent_id = Some(root.id);
    db::upsert_task(&state.db, &predecessor).await.unwrap();

    let resp = dispatch(
        &state,
        Request {
            id: 1,
            method: method::WORKFLOW_SPAWN.to_string(),
            params: serde_json::json!({
                "name": "child",
                "root_id": root.id,
                "depends_on": [predecessor.id],
            }),
        },
    )
    .await;
    let task = resp.result.expect("spawn result");
    let id = Uuid::parse_str(task["id"].as_str().unwrap()).unwrap();
    assert_eq!(task["root_id"], root.id.to_string());
    assert_eq!(task["parent_id"], root.id.to_string());
    assert_eq!(task["status"], "pending");
    assert_eq!(
        db::list_dependencies(&state.db, id).await.unwrap(),
        vec![predecessor.id]
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn workflow_spawn_rejects_unknown_task_and_dependency() {
    let (dir, state) = dag_state("spawn-invalid", &["child"]).await;
    let root = oneshot_task();
    db::upsert_task(&state.db, &root).await.unwrap();
    let mut other_root_task = oneshot_task();
    other_root_task.root_id = Some(Uuid::new_v4());
    db::upsert_task(&state.db, &other_root_task).await.unwrap();

    let spawn = |id: u64, params: serde_json::Value| {
        let state = state.clone();
        async move {
            dispatch(
                &state,
                Request {
                    id,
                    method: method::WORKFLOW_SPAWN.to_string(),
                    params,
                },
            )
            .await
        }
    };
    let assert_invalid = |resp: Response, needle: &str| {
        let error = resp.error.expect("error object");
        assert_eq!(error.code, error_code::INVALID_PARAMS);
        assert!(error.message.contains(needle), "{error:?}");
    };

    assert_invalid(
        spawn(1, serde_json::json!({ "name": "ghost" })).await,
        "unknown task",
    );
    assert_invalid(
        spawn(
            2,
            serde_json::json!({ "name": "child", "depends_on": [Uuid::new_v4()] }),
        )
        .await,
        "not found",
    );
    assert_invalid(
        spawn(
            3,
            serde_json::json!({
                "name": "child",
                "root_id": root.id,
                "depends_on": [other_root_task.id],
            }),
        )
        .await,
        "does not belong to root",
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn workflow_inspect_marks_dynamic_dependency_blocked() {
    let (dir, state) = dag_state("inspect-dynamic", &["a", "b"]).await;
    let resp = dispatch_create(
        &state,
        1,
        create_params(
            "op-inspect",
            None,
            serde_json::json!([
                { "key": "a", "name": "a" },
                { "key": "b", "name": "b", "depends_on": ["a"] },
            ]),
        ),
    )
    .await;
    let result = resp.result.expect("create result");
    let root = Uuid::parse_str(result["root_id"].as_str().unwrap()).unwrap();
    let id_a = node_id(&result, "a");
    let id_b = node_id(&result, "b");

    let inspect = |id: u64| {
        let state = state.clone();
        async move {
            dispatch(
                &state,
                Request {
                    id,
                    method: method::WORKFLOW_INSPECT.to_string(),
                    params: serde_json::json!({ "root_id": root }),
                },
            )
            .await
            .result
            .expect("inspect result")
        }
    };

    let view = inspect(2).await;
    assert_eq!(id_list(&view, "ready"), vec![id_a]);
    assert_eq!(id_list(&view, "blocked"), vec![id_b]);

    // Finish the predecessor: the dependent moves from `blocked` to `ready`.
    let mut a = db::get_task(&state.db, id_a).await.unwrap().unwrap();
    a.status = TaskStatus::Succeeded;
    a.finished_at = Some(Timestamp::now());
    db::upsert_task(&state.db, &a).await.unwrap();
    let view = inspect(3).await;
    assert!(id_list(&view, "blocked").is_empty());
    assert_eq!(id_list(&view, "ready"), vec![id_b]);

    let _ = std::fs::remove_dir_all(&dir);
}

// --- HTTP `POST /rpc` (bearer auth + MessagePack request/response) -------------
/// Drive `app` with a single `POST /rpc` and collect status, headers and body.
async fn post_rpc(
    app: axum::Router,
    body: Vec<u8>,
    token: Option<&str>,
) -> (axum::http::StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut builder = axum::http::Request::builder()
        .method("POST")
        .uri("/rpc")
        .header(axum::http::header::CONTENT_TYPE, "application/x-msgpack");
    if let Some(t) = token {
        builder = builder.header(axum::http::header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let res = app
        .oneshot(builder.body(axum::body::Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, headers, bytes.to_vec())
}

/// A scratch state plus the production `/rpc` router, for HTTP transport tests.
async fn rpc_state(tag: &str) -> (std::path::PathBuf, Arc<State>, axum::Router) {
    let dir = temp_dir(tag);
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let state = test_state(&dir, &tasks_dir).await;
    let app = crate::transport::routes().with_state(state.clone());
    (dir, state, app)
}

fn request_frame(id: u64, method_name: &str) -> Vec<u8> {
    request_frame_with_params(id, method_name, serde_json::json!({}))
}

fn request_frame_with_params(id: u64, method_name: &str, params: serde_json::Value) -> Vec<u8> {
    favetto_core::wire::encode(&Frame::Request(Request {
        id,
        method: method_name.to_string(),
        params,
    }))
    .unwrap()
}

#[tokio::test]
async fn http_rpc_round_trips_a_request() {
    let (dir, state, app) = rpc_state("http-rpc-round-trip").await;
    let body = request_frame(42, method::TASKS_LIST);

    let (status, headers, bytes) = post_rpc(app, body, Some(state.token.as_str())).await;

    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(
        headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/x-msgpack")
    );
    let frame = favetto_core::wire::decode(&bytes).expect("response frame decodes");
    match frame {
        Frame::Response(resp) => {
            assert_eq!(resp.id, 42);
            assert!(resp.error.is_none(), "{:?}", resp.error);
            assert!(
                resp.result.as_ref().is_some_and(|r| r.is_array()),
                "tasks.list should be an array: {:?}",
                resp.result
            );
        }
        other => panic!("expected a response frame, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn http_rpc_requires_a_bearer_token() {
    let (dir, state, app) = rpc_state("http-rpc-auth").await;
    let body = request_frame(1, method::PING);

    let (missing, _, _) = post_rpc(app, body.clone(), None).await;
    assert_eq!(missing, axum::http::StatusCode::UNAUTHORIZED);

    let app = crate::transport::routes().with_state(state.clone());
    let (wrong, _, _) = post_rpc(app, body, Some("not-the-token")).await;
    assert_eq!(wrong, axum::http::StatusCode::UNAUTHORIZED);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn http_rpc_rejects_a_malformed_body() {
    let (dir, state, app) = rpc_state("http-rpc-malformed").await;

    let (status, _, _) = post_rpc(app, b"not msgpack".to_vec(), Some(state.token.as_str())).await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn http_rpc_rejects_a_non_request_frame() {
    let (dir, state, app) = rpc_state("http-rpc-non-request").await;
    let body = favetto_core::wire::encode(&Frame::Notification(Notification {
        method: "event".to_string(),
        params: serde_json::Value::Null,
    }))
    .unwrap();

    let (status, _, _) = post_rpc(app, body, Some(state.token.as_str())).await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn http_rpc_reports_protocol_errors_inside_the_response() {
    let (dir, state, app) = rpc_state("http-rpc-protocol-error").await;
    let body = request_frame(7, "no.such.method");

    let (status, _, bytes) = post_rpc(app, body, Some(state.token.as_str())).await;
    assert_eq!(status, axum::http::StatusCode::OK);

    match favetto_core::wire::decode(&bytes).expect("response frame decodes") {
        Frame::Response(resp) => {
            assert_eq!(resp.id, 7);
            assert!(resp.result.is_none());
            let error = resp.error.expect("an error frame");
            assert_eq!(error.code, error_code::METHOD_NOT_FOUND);
        }
        other => panic!("expected a response frame, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// The stateful agent/task methods are intercepted by `handle_request` so they
/// can subscribe a socket/WebSocket connection. HTTP `POST /rpc` has no
/// connection scope, so they must still dispatch through the shared helpers
/// instead of falling through to `METHOD_NOT_FOUND`.
#[tokio::test]
async fn http_rpc_dispatches_connection_scoped_methods() {
    let (dir, state, app) = rpc_state("http-rpc-scoped-methods").await;
    let token = state.token.as_str().to_string();

    // Each entry is (method, params, expected error code). The calls fail
    // cheaply on missing agents/sessions, but they must be *dispatched*.
    let cases: Vec<(&str, serde_json::Value, i32)> = vec![
        (
            method::AGENTS_START,
            serde_json::json!({ "agent": "missing" }),
            error_code::INVALID_PARAMS,
        ),
        (
            method::AGENTS_ATTACH,
            serde_json::json!({ "session_id": "missing" }),
            error_code::INTERNAL,
        ),
        (
            method::AGENTS_LOAD,
            serde_json::json!({ "session_id": "missing" }),
            error_code::INVALID_PARAMS,
        ),
        (
            method::AGENTS_PROVIDER_RESUME,
            serde_json::json!({ "agent": "missing", "provider_session_id": "missing" }),
            error_code::INVALID_PARAMS,
        ),
        (
            method::TASKS_START_ONESHOT,
            serde_json::json!({}),
            error_code::INTERNAL,
        ),
    ];

    for (index, (name, params, expected)) in cases.into_iter().enumerate() {
        let body = request_frame_with_params(index as u64, name, params);
        let (status, _, bytes) = post_rpc(app.clone(), body, Some(&token)).await;
        assert_eq!(status, axum::http::StatusCode::OK, "{name}");

        match favetto_core::wire::decode(&bytes).expect("response frame decodes") {
            Frame::Response(resp) => {
                assert_eq!(resp.id, index as u64, "{name}");
                let error = resp
                    .error
                    .unwrap_or_else(|| panic!("{name} should report a domain error"));
                assert_ne!(
                    error.code,
                    error_code::METHOD_NOT_FOUND,
                    "{name} must be dispatched over HTTP"
                );
                assert_eq!(error.code, expected, "{name}: {error:?}");
            }
            other => panic!("{name}: expected a response frame, got {other:?}"),
        }
    }

    // `agents.close` is idempotent: closing an unknown id still succeeds and
    // echoes the id back, so it too is dispatched rather than unknown.
    let body = request_frame_with_params(
        98,
        method::AGENTS_CLOSE,
        serde_json::json!({ "session_id": "missing" }),
    );
    let (status, _, bytes) = post_rpc(app.clone(), body, Some(&token)).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    match favetto_core::wire::decode(&bytes).expect("response frame decodes") {
        Frame::Response(resp) => {
            assert!(resp.error.is_none(), "{:?}", resp.error);
            assert_eq!(
                resp.result.expect("agents.close result")["closed"],
                serde_json::json!("missing")
            );
        }
        other => panic!("expected a response frame, got {other:?}"),
    }

    // `events.subscribe` has no replay to stream over a one-shot request, but
    // it must still acknowledge the subscription.
    let body = request_frame(99, method::EVENTS_SUBSCRIBE);
    let (status, _, bytes) = post_rpc(app, body, Some(&token)).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    match favetto_core::wire::decode(&bytes).expect("response frame decodes") {
        Frame::Response(resp) => {
            assert_eq!(resp.id, 99);
            assert!(resp.error.is_none(), "{:?}", resp.error);
            assert_eq!(
                resp.result.expect("events.subscribe result")["subscribed"],
                serde_json::json!(true)
            );
        }
        other => panic!("expected a response frame, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn http_rpc_rejects_a_body_over_the_frame_limit() {
    let dir = temp_dir("http-rpc-limit");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let state = test_state(&dir, &tasks_dir).await;
    // A tiny limit pins the `DefaultBodyLimit` layer without allocating 64 MiB.
    let app = crate::transport::rpc_routes(1024).with_state(state.clone());

    let (status, _, _) = post_rpc(app, vec![0u8; 2048], Some(state.token.as_str())).await;
    assert_eq!(status, axum::http::StatusCode::PAYLOAD_TOO_LARGE);

    let _ = std::fs::remove_dir_all(&dir);
}
// ---------------------------------------------------------------------------
// SSE: `GET /events` and the shared `EventBus::resumable` helper
// ---------------------------------------------------------------------------
/// `Authorization: Bearer <token>` for `state`'s token.
fn auth_header(state: &State) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_str(&format!("Bearer {}", state.token.as_str()))
            .expect("valid bearer header"),
    );
    headers
}

/// Collect SSE text until `done` is satisfied or a 10 s overall deadline
/// elapses. The resumable stream does not end on its own, so the predicate
/// identifies the finite replay/target frames; the per-read timeout only guards
/// a stalled stream (well above any real inter-frame gap under load).
async fn drain_sse_until(body: axum::body::Body, mut done: impl FnMut(&str) -> bool) -> String {
    let mut stream = body.into_data_stream();
    let mut text = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if done(&text) || tokio::time::Instant::now() >= deadline {
            return text;
        }
        match tokio::time::timeout(std::time::Duration::from_secs(5), stream.next()).await {
            Ok(Some(Ok(chunk))) => text.push_str(&String::from_utf8_lossy(&chunk)),
            // stream ended / errored / stalled
            _ => return text,
        }
    }
}

/// Read exactly one SSE chunk, failing if none arrives within a second.
async fn first_sse_chunk(body: axum::body::Body) -> String {
    let mut stream = body.into_data_stream();
    match tokio::time::timeout(std::time::Duration::from_secs(1), stream.next()).await {
        Ok(Some(Ok(chunk))) => String::from_utf8_lossy(&chunk).into_owned(),
        other => panic!("expected an SSE chunk, got {other:?}"),
    }
}

/// The `id:` of every `event:` frame, in order. `task.updated` / `catalog.updated`
/// frames carry no `id:` and are ignored by construction.
fn replayed_event_ids(text: &str) -> Vec<i64> {
    text.split("\n\n")
        .filter(|frame| frame.lines().any(|l| l.trim() == "event: event"))
        .filter_map(|frame| {
            frame
                .lines()
                .find_map(|l| l.strip_prefix("id: "))
                .and_then(|id| id.trim().parse::<i64>().ok())
        })
        .collect()
}

/// Call the handler with the given resume inputs and drain until `expected`
/// durable events have been replayed.
async fn sse_replay_text(
    state: &Arc<State>,
    headers: HeaderMap,
    query: EventsQuery,
    expected: usize,
) -> String {
    let resp = events_handler(AxumState(state.clone()), headers, Query(query)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    drain_sse_until(resp.into_body(), |t| {
        replayed_event_ids(t).len() >= expected
    })
    .await
}

#[tokio::test]
async fn sse_started_task_emits_id_event_and_data() {
    let dir = temp_dir("sse-start");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(tasks_dir.join("issue.md"), "agent = \"x\"\n---\nprompt\n").unwrap();
    let state = test_state(&dir, &tasks_dir).await;

    let resp = events_handler(
        AxumState(state.clone()),
        auth_header(&state),
        Query(EventsQuery::default()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    let body = resp.into_body();

    // Starting a catalog task publishes `task.updated` then emits a durable
    // `TaskIdle` event on the same bus.
    let started = dispatch(
        &state,
        Request {
            id: 1,
            method: method::TASKS_START.to_string(),
            params: serde_json::json!({ "name": "issue" }),
        },
    )
    .await;
    assert!(started.error.is_none(), "{:?}", started.error);

    let text = drain_sse_until(body, |t| {
        t.contains("event: event")
            && t.contains("event: task.updated")
            && t.contains("\"kind\":\"task_idle\"")
    })
    .await;
    let mut saw_event = false;
    let mut saw_task_updated = false;
    for frame in text.split("\n\n") {
        if frame.lines().any(|l| l.trim() == "event: event") {
            saw_event = true;
            assert!(
                frame.lines().any(|l| l.starts_with("id: ")),
                "an event frame must carry id: {frame}"
            );
            assert!(
                frame.contains("data: "),
                "an event frame must carry data: {frame}"
            );
        }
        if frame.lines().any(|l| l.trim() == "event: task.updated") {
            saw_task_updated = true;
            assert!(
                !frame.lines().any(|l| l.starts_with("id: ")),
                "task.updated must not advance the cursor: {frame}"
            );
        }
    }
    assert!(saw_event, "no `event:` frame arrived: {text}");
    assert!(saw_task_updated, "no `task.updated` frame arrived: {text}");
    assert!(text.contains("\"kind\":\"task_idle\""), "{text}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn sse_replays_from_header_and_query() {
    let dir = temp_dir("sse-replay");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let state = test_state(&dir, &tasks_dir).await;

    for _ in 0..3 {
        state
            .emit_event(EventKind::TaskIdle, serde_json::json!({}))
            .await;
    }

    // `Last-Event-ID: 1` replays exactly {2, 3}.
    let mut headers = auth_header(&state);
    headers.insert("last-event-id", axum::http::HeaderValue::from_static("1"));
    let text = sse_replay_text(&state, headers, EventsQuery::default(), 2).await;
    assert_eq!(replayed_event_ids(&text), vec![2, 3], "{text}");

    // `?last_event_id=2` replays exactly {3}.
    let text = sse_replay_text(
        &state,
        auth_header(&state),
        EventsQuery {
            last_event_id: Some(2),
        },
        1,
    )
    .await;
    assert_eq!(replayed_event_ids(&text), vec![3], "{text}");

    // The header wins over the query: 1 -> {2, 3}, not {3}.
    let mut headers = auth_header(&state);
    headers.insert("last-event-id", axum::http::HeaderValue::from_static("1"));
    let text = sse_replay_text(
        &state,
        headers,
        EventsQuery {
            last_event_id: Some(2),
        },
        2,
    )
    .await;
    assert_eq!(replayed_event_ids(&text), vec![2, 3], "{text}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn sse_keep_alive_is_a_comment() {
    // A live sender with no pushes: only the keep-alive can arrive.
    let (_tx, rx) = mpsc::channel::<ServerPush>(4);
    let resp = sse_response(rx, std::time::Duration::from_millis(10));
    let chunk = first_sse_chunk(resp.into_body()).await;
    assert!(chunk.contains(": keep-alive"), "{chunk}");
}

#[tokio::test]
async fn events_subscribe_uses_the_shared_helper() {
    let dir = temp_dir("events-subscribe-shared");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let state = test_state(&dir, &tasks_dir).await;
    state
        .emit_event(EventKind::TaskIdle, serde_json::json!({ "n": 1 }))
        .await;

    let (tx, mut rx) = mpsc::channel::<Frame>(16);
    let subscribed: Attached = Arc::new(Mutex::new(HashSet::new()));
    let live_task: LiveTask = Arc::new(Mutex::new(None));

    let subscribe = |id: u64, cursor: i64| Request {
        id,
        method: method::EVENTS_SUBSCRIBE.to_string(),
        params: serde_json::json!({ "last_event_id": cursor }),
    };

    // Subscribing from cursor 0 makes the shared helper replay event 1 before
    // switching to live delivery.
    handle_request(&state, &tx, &subscribed, &live_task, subscribe(1, 0)).await;
    assert!(matches!(rx.recv().await.expect("ack"), Frame::Response(_)));
    match rx.recv().await.expect("a replayed event") {
        Frame::Notification(n) => {
            assert_eq!(n.method, push::EVENT);
            assert_eq!(n.params["id"], 1);
        }
        other => panic!("expected a replayed event, got {other:?}"),
    }

    // A live push after the replay reaches the same connection.
    state.bus.publish(ServerPush::CatalogUpdated);
    match rx.recv().await.expect("a live push") {
        Frame::Notification(n) => assert_eq!(n.method, push::CATALOG_UPDATED),
        other => panic!("expected a live push, got {other:?}"),
    }

    // Re-subscribing from the same cursor restarts the pusher rather than adding
    // a second one: one publish yields exactly one notification.
    handle_request(&state, &tx, &subscribed, &live_task, subscribe(2, 1)).await;
    assert!(matches!(
        rx.recv().await.expect("second ack"),
        Frame::Response(_)
    ));
    state.bus.publish(ServerPush::CatalogUpdated);
    assert!(matches!(
        rx.recv().await.expect("one live push"),
        Frame::Notification(_)
    ));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .is_err(),
        "a re-subscribe must not leave the previous pusher running"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn events_handler_rejects_missing_or_bad_token() {
    let dir = temp_dir("sse-auth");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let state = test_state(&dir, &tasks_dir).await;

    let resp = events_handler(
        AxumState(state.clone()),
        HeaderMap::new(),
        Query(EventsQuery::default()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_static("Bearer wrong"),
    );
    let resp = events_handler(
        AxumState(state.clone()),
        headers,
        Query(EventsQuery::default()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let resp = events_handler(
        AxumState(state.clone()),
        auth_header(&state),
        Query(EventsQuery::default()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Dispatch a request and unwrap the `result` value.
async fn call_ok(state: &Arc<State>, method: &str, params: serde_json::Value) -> serde_json::Value {
    let req = Request {
        id: 1,
        method: method.to_string(),
        params,
    };
    let resp = dispatch(state, req).await;
    assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
    resp.result.expect("result")
}

/// Create a repo with a `.favetto/tasks` catalog and return its path.
fn make_repo(root: &Path, name: &str) -> std::path::PathBuf {
    let repo = root.join(name);
    std::fs::create_dir_all(repo.join(".favetto/tasks")).unwrap();
    std::fs::write(
        repo.join(".favetto/tasks/triage.md"),
        "agent = \"x\"\n---\nrepo prompt\n",
    )
    .unwrap();
    repo
}

#[tokio::test]
async fn repos_dispatch_add_list_activate_remove() {
    let dir = temp_dir("repos-dispatch");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let repo = make_repo(&dir, "favetto");
    let state = test_state(&dir, &tasks_dir).await;

    // Add.
    let added = call_ok(
        &state,
        method::REPOS_ADD,
        serde_json::json!({ "path": repo.to_string_lossy() }),
    )
    .await;
    assert_eq!(added["name"], "favetto");
    assert_eq!(added["available"], true);
    assert_eq!(added["active"], false);
    assert_eq!(added["source"], "manual");

    // List includes the repo and a null active repo.
    let list = call_ok(&state, method::REPOS_LIST, serde_json::json!({})).await;
    assert_eq!(list["repos"].as_array().unwrap().len(), 1);
    assert_eq!(list["repos"][0]["name"], "favetto");
    assert_eq!(list["repos"][0]["source"], "manual");
    assert!(list["active"].is_null());

    // catalog.list exposes the repo field and the retargeted catalog.
    let catalog = call_ok(&state, method::CATALOG_LIST, serde_json::json!({})).await;
    let entry = catalog
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "repo:favetto/triage")
        .expect("repo task in catalog");
    assert_eq!(entry["repo"], "favetto");
    assert_eq!(entry["cwd"], repo.to_string_lossy().as_ref());

    // catalog.get reads the repo file.
    let got = call_ok(
        &state,
        method::CATALOG_GET,
        serde_json::json!({ "name": "repo:favetto/triage" }),
    )
    .await;
    assert!(got["markdown"].as_str().unwrap().contains("repo prompt"));

    // Activate.
    let activated = call_ok(
        &state,
        method::REPOS_ACTIVATE,
        serde_json::json!({ "name": "favetto" }),
    )
    .await;
    assert_eq!(activated["active"], "favetto");
    assert_eq!(activated["repos"][0]["active"], true);

    // Remove clears the active repo.
    let removed = call_ok(
        &state,
        method::REPOS_REMOVE,
        serde_json::json!({ "name": "favetto" }),
    )
    .await;
    assert_eq!(removed["removed"], true);
    let list = call_ok(&state, method::REPOS_LIST, serde_json::json!({})).await;
    assert!(list["repos"].as_array().unwrap().is_empty());
    assert!(list["active"].is_null());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn catalog_add_and_update_with_a_repo_qualified_name() {
    let dir = temp_dir("repos-catalog-add");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let repo = make_repo(&dir, "favetto");
    let state = test_state(&dir, &tasks_dir).await;
    call_ok(
        &state,
        method::REPOS_ADD,
        serde_json::json!({ "path": repo.to_string_lossy() }),
    )
    .await;

    // Add a task into the active repo by its qualified name.
    let added = call_ok(
        &state,
        method::CATALOG_ADD,
        serde_json::json!({
            "name": "repo:favetto/new-task",
            "agent": "x",
            "prompt": "hello",
        }),
    )
    .await;
    assert_eq!(added["name"], "repo:favetto/new-task");
    assert!(repo.join(".favetto/tasks/new-task.md").exists());
    assert!(!tasks_dir.join("repo:favetto/new-task.md").exists());

    // Updating it writes back to the same repo file.
    call_ok(
        &state,
        method::CATALOG_UPDATE,
        serde_json::json!({
            "name": "repo:favetto/new-task",
            "markdown": "agent = \"x\"\n---\nupdated\n",
        }),
    )
    .await;
    let contents = std::fs::read_to_string(repo.join(".favetto/tasks/new-task.md")).unwrap();
    assert!(contents.contains("updated"));

    // An unknown repo is rejected.
    let req = Request {
        id: 2,
        method: method::CATALOG_ADD.to_string(),
        params: serde_json::json!({ "name": "repo:missing/x", "agent": "x", "prompt": "p" }),
    };
    let resp = dispatch(&state, req).await;
    assert!(resp.error.is_some());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn repos_reject_malformed_params() {
    let dir = temp_dir("repos-malformed");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let state = test_state(&dir, &tasks_dir).await;

    // `repos.add` needs a path.
    let req = Request {
        id: 1,
        method: method::REPOS_ADD.to_string(),
        params: serde_json::json!({}),
    };
    let resp = dispatch(&state, req).await;
    let error = resp.error.expect("error");
    assert_eq!(error.code, favetto_core::rpc::error_code::INVALID_PARAMS);

    // `repos.remove` needs a name.
    let req = Request {
        id: 2,
        method: method::REPOS_REMOVE.to_string(),
        params: serde_json::json!({}),
    };
    let resp = dispatch(&state, req).await;
    let error = resp.error.expect("error");
    assert_eq!(error.code, favetto_core::rpc::error_code::INVALID_PARAMS);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn repos_refresh_discovers_scanned_repos() {
    let dir = temp_dir("repos-refresh");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let base = dir.join("code");
    make_repo(&base, "first");

    let mut config = crate::config::FavettoConfig::default();
    config.repos.base_dir = Some(base.clone());
    let state = test_state_with_config(&dir, &tasks_dir, config).await;

    // The first refresh discovers the repo as scanned.
    let list = call_ok(&state, method::REPOS_REFRESH, serde_json::json!({})).await;
    assert_eq!(list["repos"].as_array().unwrap().len(), 1);
    assert_eq!(list["repos"][0]["name"], "first");
    assert_eq!(list["repos"][0]["source"], "scanned");

    // A new directory appears and a second refresh picks it up.
    let second = make_repo(&base, "second");
    let list = call_ok(&state, method::REPOS_REFRESH, serde_json::json!({})).await;
    let names: Vec<&str> = list["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["first", "second"]);

    // Removing the directory drops it on the next refresh.
    std::fs::remove_dir_all(&second).unwrap();
    let list = call_ok(&state, method::REPOS_REFRESH, serde_json::json!({})).await;
    assert_eq!(list["repos"].as_array().unwrap().len(), 1);
    assert_eq!(list["repos"][0]["name"], "first");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn tasks_start_accepts_a_repo_qualified_name() {
    let dir = temp_dir("repos-start");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let repo = make_repo(&dir, "favetto");
    let state = test_state(&dir, &tasks_dir).await;
    call_ok(
        &state,
        method::REPOS_ADD,
        serde_json::json!({ "path": repo.to_string_lossy() }),
    )
    .await;

    let started = call_ok(
        &state,
        method::TASKS_START,
        serde_json::json!({ "name": "repo:favetto/triage" }),
    )
    .await;
    assert_eq!(started["name"], "repo:favetto/triage");

    let _ = std::fs::remove_dir_all(&dir);
}
