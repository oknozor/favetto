use super::*;
use favetto_core::rpc::error_code;
use std::path::Path;

use crate::state::StateInit;

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
    let pool = crate::db::open(&dir.join("test.db")).await.unwrap();
    crate::db::migrate(&pool).await.unwrap();
    let catalog = Arc::new(parking_lot::RwLock::new(
        crate::tasks::load_catalog(tasks_dir).unwrap(),
    ));
    let scheduler = tokio_cron_scheduler::JobScheduler::new().await.unwrap();
    Arc::new(State::new(StateInit {
        db: pool,
        bus: crate::event_bus::EventBus::new(64),
        token: favetto_core::auth::Token::generate(),
        webhooks: crate::webhooks::WebhookSecrets::from_config(
            &crate::config::FavettoConfig::default(),
        ),
        agents: crate::agents::AgentManager::new(),
        registry: crate::agents::AgentRegistry::default(),
        config: Arc::new(crate::config::FavettoConfig::default()),
        data_dir: dir.to_path_buf(),
        tasks_dir: tasks_dir.to_path_buf(),
        catalog,
        scheduler,
        hook_store: Arc::new(parking_lot::RwLock::new(Vec::new())),
    }))
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
    child_fail.finished_at = Some(Utc::now());
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
    child_done.finished_at = Some(Utc::now());
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
    done.finished_at = Some(Utc::now());
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
    failed.finished_at = Some(Utc::now());
    db::upsert_task(&state.db, &failed).await.unwrap();
    db::insert_task_run(
        &state.db,
        &TaskRun {
            id: Uuid::new_v4(),
            task_id: failed.id,
            attempt: 1,
            status: RunStatus::Failed,
            agent: None,
            session_id: None,
            started_at: Some(Utc::now()),
            finished_at: Some(Utc::now()),
            exit_code: None,
            error: None,
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
    let def = add_catalog_task(&state, &params).await.unwrap();
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
    add_catalog_task(&state, &params).await.unwrap();

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
    add_catalog_task(&state, &params).await.unwrap();

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
async fn update_catalog_task_rewrites_file_and_publishes() {
    let dir = temp_dir("update");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(tasks_dir.join("a.md"), "agent = \"x\"\n---\noriginal\n").unwrap();
    let state = test_state(&dir, &tasks_dir).await;
    let mut rx = state.bus.subscribe();

    let updated = "agent = \"x\"\n---\nupdated body\n";
    let params = serde_json::json!({ "name": "a", "markdown": updated });
    update_catalog_task(&state, &params).await.unwrap();

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
    update_catalog_task(&state, &params).await.unwrap();

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
    let err = update_catalog_task(&state, &params).await.unwrap_err();
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
    let err = update_catalog_task(&state, &params).await.unwrap_err();
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
    let err = update_catalog_task(&state, &params).await.unwrap_err();
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
fn session_list_script(dir: &Path, json: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("opencode-session-list.sh");
    std::fs::write(&path, format!("#!/bin/sh\nprintf '%s' '{json}'\n")).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

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
    let scheduler = tokio_cron_scheduler::JobScheduler::new().await.unwrap();
    Arc::new(State::new(StateInit {
        db: pool,
        bus: crate::event_bus::EventBus::new(64),
        token: favetto_core::auth::Token::generate(),
        webhooks: crate::webhooks::WebhookSecrets::from_config(&cfg),
        agents: crate::agents::AgentManager::new(),
        registry,
        config: Arc::new(cfg),
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
        created_at: Utc::now(),
        started_at: Some(Utc::now()),
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

#[cfg(unix)]
#[tokio::test]
async fn finish_oneshot_records_session_id_and_title() {
    let dir = temp_dir("oneshot-title");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let script = session_list_script(&dir, r#"[{"id":"ses_1","title":"Fixture title"}]"#);
    let state = title_state(&dir, &tasks_dir, &script).await;

    let task = oneshot_task();
    db::insert_task(&state.db, &task).await.unwrap();

    finish_oneshot(
        &state,
        task.id,
        "opencode",
        Some("ses_1".to_string()),
        &dir,
        Some(0),
    )
    .await;

    let stored = db::get_task(&state.db, task.id).await.unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::Succeeded);
    assert_eq!(stored.session_id.as_deref(), Some("ses_1"));
    assert_eq!(stored.session_title.as_deref(), Some("Fixture title"));
    assert!(stored.error.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[tokio::test]
async fn finish_oneshot_failure_keeps_session_info() {
    let dir = temp_dir("oneshot-title-fail");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let script = session_list_script(&dir, r#"[{"id":"ses_1","title":"Fixture title"}]"#);
    let state = title_state(&dir, &tasks_dir, &script).await;

    let task = oneshot_task();
    db::insert_task(&state.db, &task).await.unwrap();

    finish_oneshot(
        &state,
        task.id,
        "opencode",
        Some("ses_1".to_string()),
        &dir,
        Some(1),
    )
    .await;

    let stored = db::get_task(&state.db, task.id).await.unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::Failed);
    assert_eq!(stored.session_id.as_deref(), Some("ses_1"));
    assert_eq!(stored.session_title.as_deref(), Some("Fixture title"));
    assert!(stored.error.is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

/// `finish_oneshot` emits the same enriched `TaskFinished` payload as the
/// executor, so the event shape does not depend on how a task was run.
#[cfg(unix)]
#[tokio::test]
async fn finish_oneshot_emits_an_enriched_finished_event() {
    let dir = temp_dir("oneshot-enriched");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let script = session_list_script(&dir, r#"[{"id":"ses_1","title":"Fixture title"}]"#);
    let state = title_state(&dir, &tasks_dir, &script).await;

    let task = oneshot_task();
    db::insert_task(&state.db, &task).await.unwrap();

    finish_oneshot(
        &state,
        task.id,
        "opencode",
        Some("ses_1".to_string()),
        &dir,
        Some(1),
    )
    .await;

    let events = db::tail_events(&state.db, 10).await.unwrap();
    let finished = events
        .iter()
        .rev()
        .find(|e| e.kind == EventKind::TaskFinished)
        .expect("TaskFinished emitted");
    assert_eq!(finished.payload["name"], task.name);
    assert_eq!(finished.payload["task_id"], task.id.to_string());
    assert_eq!(finished.payload["success"], false);
    assert_eq!(finished.payload["status"], "failed");
    assert_eq!(finished.payload["attempt"], 1);
    assert_eq!(finished.payload["retryable"], false);
    assert_eq!(finished.payload["summary"], "agent session exited with 1");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[tokio::test]
async fn finish_oneshot_without_session_id_is_blank() {
    let dir = temp_dir("oneshot-no-session");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let script = session_list_script(&dir, r#"[{"id":"ses_1","title":"Fixture title"}]"#);
    let state = title_state(&dir, &tasks_dir, &script).await;

    let task = oneshot_task();
    db::insert_task(&state.db, &task).await.unwrap();

    finish_oneshot(&state, task.id, "opencode", None, &dir, Some(0)).await;

    let stored = db::get_task(&state.db, task.id).await.unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::Succeeded);
    assert!(stored.session_id.is_none());
    assert!(stored.session_title.is_none());
    assert!(stored.error.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A title persisted mid-run by the watcher must survive `finish_oneshot`
/// when the exit-time lookup finds nothing.
#[cfg(unix)]
#[tokio::test]
async fn finish_oneshot_preserves_mid_run_title() {
    let dir = temp_dir("oneshot-preserve");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    // The exit-time lookup finds no sessions, so it yields no title.
    let script = session_list_script(&dir, "[]");
    let state = title_state(&dir, &tasks_dir, &script).await;

    let mut task = oneshot_task();
    task.session_id = Some("ses_1".to_string());
    task.session_title = Some("Persisted mid-run".to_string());
    db::insert_task(&state.db, &task).await.unwrap();

    finish_oneshot(
        &state,
        task.id,
        "opencode",
        Some("ses_1".to_string()),
        &dir,
        Some(0),
    )
    .await;

    let stored = db::get_task(&state.db, task.id).await.unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::Succeeded);
    assert_eq!(stored.session_id.as_deref(), Some("ses_1"));
    assert_eq!(stored.session_title.as_deref(), Some("Persisted mid-run"));
    let _ = std::fs::remove_dir_all(&dir);
}

fn live_session(running: bool, headless: bool, awaiting: bool) -> AgentSessionInfo {
    AgentSessionInfo {
        id: "s1".to_string(),
        agent: "opencode".to_string(),
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
        }),
        activity: None,
        usage: None,
    }
}

#[test]
fn should_attach_to_live_allows_awaiting_headless_session() {
    // Live interactive TUI: attach.
    assert!(should_attach_to_live(&live_session(true, false, false)));
    // Running headless without a prompt: this check declines; `start_agent`
    // keeps the retained PTY read-only until the run exits.
    assert!(!should_attach_to_live(&live_session(true, true, false)));
    // Headless but blocked on the user: attach so keystrokes reach it.
    assert!(should_attach_to_live(&live_session(true, true, true)));
    // Exited sessions are never attached.
    assert!(!should_attach_to_live(&live_session(false, true, true)));
    assert!(!should_attach_to_live(&live_session(false, false, false)));
}

#[cfg(unix)]
#[tokio::test]
async fn finish_oneshot_accepts_awaiting_input() {
    let dir = temp_dir("oneshot-awaiting");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let script = session_list_script(&dir, r#"[{"id":"ses_1","title":"Fixture title"}]"#);
    let state = title_state(&dir, &tasks_dir, &script).await;

    let mut task = oneshot_task();
    task.status = TaskStatus::AwaitingInput;
    db::insert_task(&state.db, &task).await.unwrap();

    finish_oneshot(
        &state,
        task.id,
        "opencode",
        Some("ses_1".to_string()),
        &dir,
        Some(0),
    )
    .await;

    // A task paused on input is still finished once the session exits.
    let stored = db::get_task(&state.db, task.id).await.unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::Succeeded);
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[tokio::test]
async fn agents_start_renders_catalog_prompt_with_task_input() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir("start-render");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(
        tasks_dir.join("issue.md"),
        "agent = \"opencode\"\n\
             [[vars]]\nname = \"repo\"\nprompt = \"Repo\"\nrequired = true\n\
             ---\nTarget repository: `{{ input.repo }}`\n",
    )
    .unwrap();

    // A fake opencode that records the argv it was launched with.
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

    let state = title_state(&dir, &tasks_dir, &script).await;

    let task = Task {
        id: Uuid::new_v4(),
        name: "issue".to_string(),
        status: TaskStatus::Failed,
        attempt: 0,
        input: serde_json::json!({ "repo": "acme/widgets" }),
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
    db::insert_task(&state.db, &task).await.unwrap();

    // The client sends the raw catalog prompt; the server must render it
    // against the task's collected input before seeding the session.
    start_agent(
        &state,
        &serde_json::json!({
            "task_id": task.id.to_string(),
            "prompt": "Target repository: `{{ input.repo }}`",
            "rows": 40,
            "cols": 120,
        }),
    )
    .await
    .expect("start_agent");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let captured = loop {
        let text = std::fs::read_to_string(&capture).unwrap_or_default();
        if !text.is_empty() || std::time::Instant::now() >= deadline {
            break text;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert!(
        captured.contains("acme/widgets"),
        "input.repo was not rendered into the seeded prompt: {captured}"
    );
    assert!(
        !captured.contains("{{ input."),
        "a raw placeholder survived into the seeded prompt: {captured}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The panel session must inherit the catalog task's `provider`/`model`, so
/// an interactive reattach uses the same route as the headless run.
#[cfg(unix)]
#[tokio::test]
async fn agents_start_uses_catalog_provider_and_model() {
    let dir = temp_dir("start-model");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(
        tasks_dir.join("issue.md"),
        "agent = \"opencode\"\nprovider = \"acme\"\nmodel = \"big\"\n---\nHello\n",
    )
    .unwrap();

    let capture = dir.join("captured-args.txt");
    let script = argv_capture_script(&dir, &capture);
    let state = title_state(&dir, &tasks_dir, &script).await;

    let task = Task {
        id: Uuid::new_v4(),
        name: "issue".to_string(),
        status: TaskStatus::Failed,
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
    db::insert_task(&state.db, &task).await.unwrap();

    start_agent(
        &state,
        &serde_json::json!({ "task_id": task.id.to_string() }),
    )
    .await
    .expect("start_agent");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let captured = loop {
        let text = std::fs::read_to_string(&capture).unwrap_or_default();
        if !text.is_empty() || std::time::Instant::now() >= deadline {
            break text;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert!(
        captured.contains("--model acme/big"),
        "the task's provider/model did not reach the seeded session: {captured}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// End-to-end regression for issue #69: starting a catalog task through
/// `tasks.start` alone runs it headlessly and submits the rendered prompt —
/// no `agents.start` / Agent-panel attach is needed.
#[cfg(unix)]
#[tokio::test]
async fn tasks_start_submits_rendered_prompt_without_attach() {
    let dir = temp_dir("tasks-start-no-attach");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(
        tasks_dir.join("issue.md"),
        "agent = \"opencode\"\n\
             [[vars]]\nname = \"repo\"\nprompt = \"Repo\"\nrequired = true\n\
             ---\nTarget repository: `{{ input.repo }}`\n",
    )
    .unwrap();

    let capture = dir.join("captured-args.txt");
    let script = argv_capture_script(&dir, &capture);
    let state = title_state(&dir, &tasks_dir, &script).await;

    // The same RPC the TUI sends: enqueue only, never `agents.start`.
    let resp = dispatch(
        &state,
        Request {
            id: 1,
            method: method::TASKS_START.to_string(),
            params: serde_json::json!({
                "name": "issue",
                "input": { "repo": "acme/widgets" },
            }),
        },
    )
    .await;
    assert!(resp.error.is_none(), "{:?}", resp.error);

    // The background dispatcher claims and runs the queued task.
    let executor = crate::executor::spawn(state.clone());

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let captured = loop {
        let text = std::fs::read_to_string(&capture).unwrap_or_default();
        if !text.is_empty() || std::time::Instant::now() >= deadline {
            break text;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    executor.abort();

    assert!(
        captured.contains("acme/widgets"),
        "the rendered prompt was not submitted by the headless run: {captured}"
    );
    assert!(
        !captured.contains("{{ input."),
        "a raw placeholder survived into the headless prompt: {captured}"
    );
    assert!(
        state.agents.sessions().iter().all(|s| s.headless),
        "an interactive session was launched: {:?}",
        state.agents.sessions()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

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

/// Opening the panel on a live unattended run must not seed a duplicate
/// session (and re-submit the prompt) for the same task: it attaches to the
/// retained headless PTY (read-only, from the client's point of view).
#[cfg(unix)]
#[tokio::test]
async fn agents_start_attaches_a_live_headless_run_without_duplicating() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir("start-no-dup");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(tasks_dir.join("issue.md"), "agent = \"plain\"\n---\nbody\n").unwrap();

    // A non-resuming agent whose runs stay alive: a live headless session
    // with no captured agent session id to resume.
    let script = dir.join("fake-plain.sh");
    std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();

    let state = agent_state(
        &dir,
        &tasks_dir,
        "plain",
        vec![(
            "plain",
            crate::config::AgentConfig {
                command: script.to_string_lossy().into_owned(),
                headless_args: Some(vec!["run".to_string(), "{prompt}".to_string()]),
                ..Default::default()
            },
        )],
    )
    .await;

    let task = Task {
        id: Uuid::new_v4(),
        name: "issue".to_string(),
        status: TaskStatus::Running,
        attempt: 0,
        input: serde_json::json!({}),
        output: None,
        dedupe_key: None,
        created_at: Utc::now(),
        started_at: Some(Utc::now()),
        finished_at: None,
        error: None,
        failure: None,
        session_id: None,
        session_title: None,
        parent_id: None,
        root_id: None,
        interactive: false,
    };
    db::insert_task(&state.db, &task).await.unwrap();

    let agent = state.registry.get_checked("plain").unwrap();
    let info = state
        .agents
        .start(
            "plain",
            agent,
            Some(task.id.to_string()),
            Invocation::Headless {
                prompt: "body",
                provider: None,
                model: None,
            },
            AgentContext {
                rows: 24,
                cols: 80,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(info.headless && info.running);

    let attached = start_agent(
        &state,
        &serde_json::json!({ "task_id": task.id.to_string(), "agent": "plain" }),
    )
    .await
    .expect("a live headless run must be attached, not duplicated");
    assert_eq!(
        attached.id, info.id,
        "start_agent must bind to the live run's PTY"
    );
    assert!(attached.headless && attached.running);

    let sessions = state.agents.sessions();
    assert_eq!(
        sessions.len(),
        1,
        "a duplicate session was launched: {sessions:?}"
    );
    assert_eq!(sessions[0].id, info.id);

    state.agents.close(&info.id).ok();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A resumable agent whose headless run is still writing its session must not
/// be resumed concurrently: `start_agent` attaches the retained PTY instead.
/// Once the run is gone, a persisted session id resumes the interactive TUI.
#[cfg(unix)]
#[tokio::test]
async fn agents_start_does_not_resume_a_live_headless_writer() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir("start-live-resume");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(tasks_dir.join("issue.md"), "agent = \"rsm\"\n---\nbody\n").unwrap();

    // A resumable agent (it has `resume_args`) whose run stays alive.
    let script = dir.join("fake-rsm.sh");
    std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();

    let state = agent_state(
        &dir,
        &tasks_dir,
        "rsm",
        vec![(
            "rsm",
            crate::config::AgentConfig {
                command: script.to_string_lossy().into_owned(),
                headless_args: Some(vec!["run".to_string(), "{prompt}".to_string()]),
                resume_args: Some(vec!["resume".to_string(), "{session_id}".to_string()]),
                ..Default::default()
            },
        )],
    )
    .await;

    let task = Task {
        id: Uuid::new_v4(),
        name: "issue".to_string(),
        status: TaskStatus::Running,
        attempt: 0,
        input: serde_json::json!({}),
        output: None,
        dedupe_key: None,
        created_at: Utc::now(),
        started_at: Some(Utc::now()),
        finished_at: None,
        error: None,
        failure: None,
        session_id: Some("ses-live".to_string()),
        session_title: None,
        parent_id: None,
        root_id: None,
        interactive: false,
    };
    db::insert_task(&state.db, &task).await.unwrap();

    let agent = state.registry.get_checked("rsm").unwrap();
    assert!(agent.capabilities().resume);
    let info = state
        .agents
        .start(
            "rsm",
            agent,
            Some(task.id.to_string()),
            Invocation::Headless {
                prompt: "body",
                provider: None,
                model: None,
            },
            AgentContext {
                session_id: Some("ses-live".to_string()),
                rows: 24,
                cols: 80,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(info.headless && info.running);
    assert_eq!(info.session_id.as_deref(), Some("ses-live"));

    let attached = start_agent(
        &state,
        &serde_json::json!({ "task_id": task.id.to_string(), "agent": "rsm" }),
    )
    .await
    .expect("a live headless writer must be attached, not resumed");
    assert_eq!(attached.id, info.id);
    assert!(attached.headless && attached.running);
    assert_eq!(
        state.agents.sessions().len(),
        1,
        "a duplicate/resumed session was launched: {:?}",
        state.agents.sessions()
    );

    // Once the writer is gone, the persisted id opens the interactive TUI.
    state.agents.close(&info.id).ok();
    let resumed = start_agent(
        &state,
        &serde_json::json!({ "task_id": task.id.to_string(), "agent": "rsm" }),
    )
    .await
    .expect("a finished resumable run must resume");
    assert_ne!(resumed.id, info.id);
    assert!(
        !resumed.headless,
        "resume must launch an interactive session"
    );

    state.agents.close(&resumed.id).ok();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression for issue #91: resuming a run's session launches the agent in
/// the run's worktree (recreating a reclaimed one), not the daemon's cwd, so
/// an agent whose session store is per-project can reopen the session.
#[cfg(unix)]
#[tokio::test]
async fn agents_start_resumes_in_the_runs_worktree() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir("resume-cwd");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();

    let run_git = |args: Vec<String>| {
        let repo = repo.clone();
        async move {
            let out = tokio::process::Command::new("git")
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

    std::fs::write(
        tasks_dir.join("issue.md"),
        format!(
            "agent = \"rsm\"\ncwd = {:?}\n---\nbody\n",
            repo.to_string_lossy()
        ),
    )
    .unwrap();

    // A resumable agent that records the directory the panel session opens in.
    let capture = dir.join("resume-pwd.txt");
    let script = dir.join("fake-rsm.sh");
    std::fs::write(&script, format!("#!/bin/sh\npwd > {}\n", capture.display())).unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();

    let mut cfg = crate::config::FavettoConfig::default();
    cfg.agent.default = Some("rsm".to_string());
    cfg.executor.parallel = true;
    cfg.agents.insert(
        "rsm".to_string(),
        crate::config::AgentConfig {
            command: script.to_string_lossy().into_owned(),
            resume_args: Some(vec!["resume".to_string(), "{session_id}".to_string()]),
            ..Default::default()
        },
    );
    let state = agent_state_with_config(&dir, &tasks_dir, cfg).await;

    let task = Task {
        id: Uuid::new_v4(),
        name: "issue".to_string(),
        status: TaskStatus::Failed,
        attempt: 0,
        input: serde_json::json!({}),
        output: None,
        dedupe_key: None,
        created_at: Utc::now(),
        started_at: None,
        finished_at: None,
        error: None,
        failure: None,
        session_id: Some("ses-1".to_string()),
        session_title: None,
        parent_id: None,
        root_id: None,
        interactive: false,
    };
    db::insert_task(&state.db, &task).await.unwrap();

    let info = start_agent(
        &state,
        &serde_json::json!({
            "task_id": task.id.to_string(),
            "agent": "rsm",
            // The TUI sends the task's base repo as `cwd`; resume must ignore
            // it in favour of the run's worktree.
            "cwd": repo.to_string_lossy(),
        }),
    )
    .await
    .expect("start_agent");
    assert!(!info.headless, "resume must launch an interactive session");

    let expected = dir.join("worktrees").join(task.id.to_string());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let captured = loop {
        let text = std::fs::read_to_string(&capture).unwrap_or_default();
        if !text.trim().is_empty() || std::time::Instant::now() >= deadline {
            break text;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert_eq!(
        std::fs::canonicalize(captured.trim()).ok(),
        std::fs::canonicalize(&expected).ok(),
        "the resumed session did not open in the run's worktree: {captured:?}"
    );
    assert!(expected.exists(), "the run's worktree was not recreated");

    state.agents.close(&info.id).ok();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Opening the panel on a finished non-resume headless run replays its final
/// screen instead of seeding a duplicate interactive session.
#[cfg(unix)]
#[tokio::test]
async fn agents_start_replays_a_finished_headless_run() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir("start-replay");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(tasks_dir.join("issue.md"), "agent = \"plain\"\n---\nbody\n").unwrap();

    // A non-resuming agent whose run finishes immediately; the daemon keeps
    // the PTY so its final screen can still be replayed.
    let script = dir.join("fake-plain.sh");
    std::fs::write(&script, "#!/bin/sh\nprintf finished-run-output\n").unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();

    let state = agent_state(
        &dir,
        &tasks_dir,
        "plain",
        vec![(
            "plain",
            crate::config::AgentConfig {
                command: script.to_string_lossy().into_owned(),
                headless_args: Some(vec!["run".to_string(), "{prompt}".to_string()]),
                ..Default::default()
            },
        )],
    )
    .await;

    let task = Task {
        id: Uuid::new_v4(),
        name: "issue".to_string(),
        status: TaskStatus::Running,
        attempt: 0,
        input: serde_json::json!({}),
        output: None,
        dedupe_key: None,
        created_at: Utc::now(),
        started_at: Some(Utc::now()),
        finished_at: None,
        error: None,
        failure: None,
        session_id: None,
        session_title: None,
        parent_id: None,
        root_id: None,
        interactive: false,
    };
    db::insert_task(&state.db, &task).await.unwrap();

    let agent = state.registry.get_checked("plain").unwrap();
    let info = state
        .agents
        .start(
            "plain",
            agent,
            Some(task.id.to_string()),
            Invocation::Headless {
                prompt: "body",
                provider: None,
                model: None,
            },
            AgentContext {
                rows: 24,
                cols: 80,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(info.headless);
    assert_eq!(state.agents.wait(&info.id).await, Some(0));

    let replayed = start_agent(
        &state,
        &serde_json::json!({ "task_id": task.id.to_string(), "agent": "plain" }),
    )
    .await
    .expect("a finished headless run must be replayed");
    assert_eq!(replayed.id, info.id, "the retained PTY must be replayed");
    assert!(replayed.headless && !replayed.running);

    let (_info, frame) = state.agents.attach(&replayed.id).unwrap();
    assert!(
        String::from_utf8_lossy(&frame).contains("finished-run-output"),
        "the finished screen was not replayed"
    );

    let sessions = state.agents.sessions();
    assert_eq!(
        sessions.len(),
        1,
        "a duplicate session was launched: {sessions:?}"
    );

    state.agents.close(&info.id).ok();
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
        created_at: Utc::now(),
        started_at: None,
        finished_at: Some(Utc::now()),
        error: None,
        failure: None,
        session_id: None,
        session_title: None,
        parent_id: None,
        root_id: None,
        interactive: false,
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
        created_at: Utc::now(),
        started_at: Some(Utc::now()),
        finished_at: Some(Utc::now()),
        error: None,
        failure: None,
        session_id: None,
        session_title: None,
        parent_id: None,
        root_id: None,
        interactive: false,
    };
    let run = |task_id: Uuid, attempt: u32, status: RunStatus| TaskRun {
        id: Uuid::new_v4(),
        task_id,
        attempt,
        status,
        agent: None,
        session_id: None,
        started_at: Some(Utc::now()),
        finished_at: None,
        exit_code: None,
        error: None,
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
#[tokio::test]
async fn dispatch_rejects_malformed_params() {
    let dir = temp_dir("malformed-params");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let state = test_state(&dir, &tasks_dir).await;

    let cases: &[(&str, serde_json::Value)] = &[
        // Missing / invalid `id`.
        (method::TASKS_GET, serde_json::json!({})),
        (method::TASKS_GET, serde_json::json!({ "id": "not-a-uuid" })),
        (method::TASKS_GET, serde_json::json!({ "id": 17 })),
        (method::TASKS_GET, serde_json::json!({ "id": null })),
        (method::TASKS_CANCEL, serde_json::json!({})),
        (
            method::TASKS_CANCEL,
            serde_json::json!({ "id": "not-a-uuid" }),
        ),
        // Missing / invalid `name`.
        (method::TASKS_START, serde_json::json!({})),
        (method::TASKS_START, serde_json::json!({ "name": 5 })),
        (method::CATALOG_GET, serde_json::json!({})),
        (method::CATALOG_GET, serde_json::json!({ "name": 5 })),
        // Other required fields.
        (method::SCHEDULES_DELETE, serde_json::json!({})),
        (method::NOTIFICATIONS_TEST, serde_json::json!({})),
        (method::AGENTS_INPUT, serde_json::json!({})),
        (method::AGENTS_RESIZE, serde_json::json!({})),
    ];

    for (case, (name, params)) in cases.iter().enumerate() {
        let resp = dispatch(
            &state,
            Request {
                id: case as u64 + 1,
                method: (*name).to_string(),
                params: params.clone(),
            },
        )
        .await;
        let err = resp
            .error
            .unwrap_or_else(|| panic!("{name} accepted malformed params {params}"));
        assert_eq!(
            err.code,
            error_code::INVALID_PARAMS,
            "{name} {params} produced {err:?}"
        );
        assert!(
            err.message.contains("invalid params"),
            "{name} {params} produced an unstable message: {:?}",
            err.message
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// Optional params keep their old lenient behaviour: an ill-typed value is
/// treated as absent so the default applies, preserving the wire contract.
#[test]
fn parse_params_tolerates_ill_typed_optional_fields() {
    let p: TasksListParams =
        parse_params(method::TASKS_LIST, &serde_json::json!({ "limit": "many" })).unwrap();
    assert_eq!(p.limit, None);
    let p: TasksListParams =
        parse_params(method::TASKS_LIST, &serde_json::json!({ "limit": 7 })).unwrap();
    assert_eq!(p.limit, Some(7));

    let p: AgentResizeParams = parse_params(
        method::AGENTS_RESIZE,
        &serde_json::json!({ "session_id": "s", "rows": true, "cols": "80" }),
    )
    .unwrap();
    assert_eq!(p.rows, None);
    assert_eq!(p.cols, None);
}

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

/// `agents.reply` deserializes its typed reply and routes it to the session's
/// `InputResponder`; an unknown session is an internal error.
#[tokio::test]
async fn dispatch_agents_reply_round_trips_against_a_state_source() {
    let dir = temp_dir("agents-reply");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let state = test_state(&dir, &tasks_dir).await;
    let fake = crate::agents::testing::fake_state_agent("while true; do sleep 1; done");
    let info = state
        .agents
        .start(
            "fake",
            fake.agent.clone(),
            None,
            Invocation::Interactive {
                prompt: None,
                provider: None,
                model: None,
            },
            AgentContext {
                rows: 40,
                cols: 120,
                ..Default::default()
            },
        )
        .unwrap();

    let reply = |id: u64, session: &str| Request {
        id,
        method: method::AGENTS_REPLY.to_string(),
        params: serde_json::json!({
            "session_id": session,
            "request_id": "perm_1",
            "reply": { "reply": "once" },
        }),
    };

    let resp = dispatch(&state, reply(1, &info.id)).await;
    assert!(resp.error.is_none(), "{:?}", resp.error);
    assert_eq!(resp.result.unwrap()["replied"], true);
    assert_eq!(
        fake.responder.replies(),
        vec![("perm_1".to_string(), InputReply::Once)]
    );

    let resp = dispatch(&state, reply(2, "missing")).await;
    assert_eq!(resp.error.expect("error").code, error_code::INTERNAL);

    state.agents.close(&info.id).ok();
    let _ = std::fs::remove_dir_all(&dir);
}

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
    a.finished_at = Some(Utc::now());
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
    a.finished_at = Some(Utc::now());
    db::upsert_task(&state.db, &a).await.unwrap();
    let view = inspect(3).await;
    assert!(id_list(&view, "blocked").is_empty());
    assert_eq!(id_list(&view, "ready"), vec![id_b]);

    let _ = std::fs::remove_dir_all(&dir);
}
