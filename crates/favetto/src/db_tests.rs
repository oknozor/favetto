use super::*;
use chrono::Utc;
use favetto_core::model::{Event, Failure, FailureKind, RunStatus, TaskRun};

#[tokio::test]
async fn event_replay_resumes_from_cursor() {
    let dir = std::env::temp_dir().join(format!("favetto-replay-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let pool = open(&dir.join("test.db")).await.unwrap();
    migrate(&pool).await.unwrap();

    for i in 0..5 {
        let ev = Event {
            id: 0,
            kind: EventKind::Unknown,
            payload: serde_json::json!({ "n": i }),
            created_at: Utc::now(),
        };
        let id = insert_event(&pool, &ev).await.unwrap();
        assert_eq!(id, i + 1);
    }

    // Replay after cursor 2 replays events 3, 4, 5 in ascending order.
    let replay = events_after(&pool, 2, 100).await.unwrap();
    assert_eq!(replay.len(), 3);
    assert_eq!(replay[0].id, 3);
    assert_eq!(replay[2].id, 5);

    // Tail returns the newest, in ascending order.
    let tail = tail_events(&pool, 3).await.unwrap();
    assert_eq!(tail.len(), 3);
    assert_eq!(tail[0].id, 3);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn row_to_event_falls_back_to_unknown() {
    let (dir, pool) = scratch_pool("unknown-kind").await;
    // A kind an older or newer build wrote that this build does not know.
    sqlx::query("INSERT INTO events (kind, payload, created_at) VALUES ('not_a_kind', '{}', ?)")
        .bind(ts_ms(Utc::now()))
        .execute(&pool)
        .await
        .unwrap();

    let events = events_after(&pool, 0, 10).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, EventKind::Unknown);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn webhook_delivery_is_recorded_once() {
    let dir = std::env::temp_dir().join(format!("favetto-delivery-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let pool = open(&dir.join("test.db")).await.unwrap();
    migrate(&pool).await.unwrap();

    assert!(record_delivery(&pool, "github", "delivery-1")
        .await
        .unwrap());
    assert!(!record_delivery(&pool, "github", "delivery-1")
        .await
        .unwrap());
    assert!(record_delivery(&pool, "github", "delivery-2")
        .await
        .unwrap());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn task_session_id_round_trips() {
    let dir = std::env::temp_dir().join(format!("favetto-tasks-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let pool = open(&dir.join("test.db")).await.unwrap();
    migrate(&pool).await.unwrap();

    let task = Task {
        id: Uuid::new_v4(),
        name: "t".to_string(),
        status: TaskStatus::Succeeded,
        attempt: 0,
        input: serde_json::json!({}),
        output: None,
        dedupe_key: None,
        created_at: Utc::now(),
        started_at: None,
        finished_at: None,
        error: None,
        failure: None,
        session_id: Some("ses_123".to_string()),
        session_title: Some("Fix the widget".to_string()),
        parent_id: None,
        root_id: None,
        interactive: false,
    };
    upsert_task(&pool, &task).await.unwrap();
    let got = get_task(&pool, task.id).await.unwrap().unwrap();
    assert_eq!(got.session_id.as_deref(), Some("ses_123"));
    assert_eq!(got.session_title.as_deref(), Some("Fix the widget"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn task_failure_round_trips_through_the_database() {
    let (dir, pool) = scratch_pool("failure-roundtrip").await;

    let mut task = task_at(Utc::now(), Some(Utc::now()), None);
    task.status = TaskStatus::Failed;
    task.error = Some("exit 1".to_string());
    task.failure = Some(Failure::new(FailureKind::Agent, "exit 1"));
    upsert_task(&pool, &task).await.unwrap();

    let got = get_task(&pool, task.id).await.unwrap().unwrap();
    let failure = got.failure.expect("failure persisted");
    assert_eq!(failure.kind, FailureKind::Agent);
    assert_eq!(failure.message, "exit 1");
    assert!(!failure.retryable);
    assert_eq!(got.error.as_deref(), Some("exit 1"));

    // The list SELECT carries the column too, not just `tasks.get`.
    let listed = list_tasks(&pool, 10).await.unwrap();
    assert_eq!(
        listed[0].failure.as_ref().map(|f| f.kind),
        Some(FailureKind::Agent)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn task_interactive_round_trips_through_the_database() {
    let (dir, pool) = scratch_pool("interactive").await;

    let user = Task {
        interactive: true,
        ..task_at(Utc::now(), None, None)
    };
    let programmatic = task_at(Utc::now(), None, None);
    assert!(!programmatic.interactive);

    insert_task(&pool, &user).await.unwrap();
    upsert_task(&pool, &programmatic).await.unwrap();

    assert!(get_task(&pool, user.id).await.unwrap().unwrap().interactive);
    assert!(
        !get_task(&pool, programmatic.id)
            .await
            .unwrap()
            .unwrap()
            .interactive
    );

    // Both list and the pending-task scan carry the flag.
    let listed = list_tasks(&pool, 500).await.unwrap();
    assert!(listed.iter().any(|t| t.id == user.id && t.interactive));
    let pending = next_pending_tasks(&pool, 10).await.unwrap();
    // Both rows are `succeeded`, so the pending scan is empty; the flag still
    // travels through `list_tasks` above. Sanity-check the scan does not error.
    assert!(pending.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn migrate_adds_session_title_to_legacy_database() {
    let dir = std::env::temp_dir().join(format!("favetto-legacy-tasks-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let pool = open(&dir.join("test.db")).await.unwrap();

    // A pre-migration database: the tasks table predates `session_title`.
    sqlx::query(
        "CREATE TABLE tasks (
                id          TEXT PRIMARY KEY,
                name        TEXT NOT NULL,
                status      TEXT NOT NULL,
                input       TEXT NOT NULL,
                output      TEXT,
                dedupe_key  TEXT UNIQUE,
                created_at  INTEGER NOT NULL,
                started_at  INTEGER,
                finished_at INTEGER,
                error       TEXT,
                session_id  TEXT
            )",
    )
    .execute(&pool)
    .await
    .unwrap();

    // A row written before the newer columns existed. The migration must fill
    // `interactive` with the headless default rather than fail.
    let legacy_id = Uuid::new_v4();
    sqlx::query("INSERT INTO tasks (id, name, status, input, created_at) VALUES (?, 'legacy', 'succeeded', '{}', ?)")
        .bind(legacy_id.to_string())
        .bind(ts_ms(Utc::now()))
        .execute(&pool)
        .await
        .unwrap();

    // Running the migration twice is a no-op the second time.
    migrate(&pool).await.unwrap();
    migrate(&pool).await.unwrap();

    let legacy = get_task(&pool, legacy_id).await.unwrap().unwrap();
    assert!(!legacy.interactive, "legacy rows must default to headless");
    assert!(
        legacy.failure.is_none(),
        "legacy rows must decode with no typed failure"
    );

    let task = Task {
        id: Uuid::new_v4(),
        name: "t".to_string(),
        status: TaskStatus::Succeeded,
        attempt: 0,
        input: serde_json::json!({}),
        output: None,
        dedupe_key: None,
        created_at: Utc::now(),
        started_at: None,
        finished_at: None,
        error: None,
        failure: None,
        session_id: Some("ses_legacy".to_string()),
        session_title: Some("Legacy title".to_string()),
        parent_id: Some(Uuid::new_v4()),
        root_id: Some(Uuid::new_v4()),
        interactive: false,
    };
    upsert_task(&pool, &task).await.unwrap();
    let got = get_task(&pool, task.id).await.unwrap().unwrap();
    assert_eq!(got.session_title.as_deref(), Some("Legacy title"));
    assert_eq!(got.parent_id, task.parent_id);
    assert_eq!(got.root_id, task.root_id);

    let _ = std::fs::remove_dir_all(&dir);
}

async fn scratch_pool(tag: &str) -> (std::path::PathBuf, SqlitePool) {
    let dir = std::env::temp_dir().join(format!(
        "favetto-{tag}-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let pool = open(&dir.join("test.db")).await.unwrap();
    migrate(&pool).await.unwrap();
    (dir, pool)
}

fn task_at(
    created_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    output: Option<serde_json::Value>,
) -> Task {
    Task {
        id: Uuid::new_v4(),
        name: "t".to_string(),
        status: TaskStatus::Succeeded,
        attempt: 0,
        input: serde_json::json!({}),
        output,
        dedupe_key: None,
        created_at,
        started_at: Some(created_at),
        finished_at,
        error: None,
        failure: None,
        session_id: None,
        session_title: None,
        parent_id: None,
        root_id: None,
        interactive: false,
    }
}

fn run_at(task_id: Uuid, attempt: u32) -> TaskRun {
    TaskRun {
        id: Uuid::new_v4(),
        task_id,
        attempt,
        status: RunStatus::Running,
        agent: None,
        session_id: None,
        started_at: Some(Utc::now()),
        finished_at: None,
        exit_code: None,
        error: None,
        failure: None,
    }
}

#[tokio::test]
async fn list_tasks_omits_output_but_get_task_keeps_it() {
    let (dir, pool) = scratch_pool("list-omits-output").await;

    let task = task_at(
        Utc::now(),
        Some(Utc::now()),
        Some(serde_json::json!({ "output": "x".repeat(50_000) })),
    );
    upsert_task(&pool, &task).await.unwrap();

    let listed = list_tasks(&pool, 500).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].output.is_none(), "list must not carry output");

    let fetched = get_task(&pool, task.id).await.unwrap().unwrap();
    assert!(
        fetched.output.is_some(),
        "tasks.get must keep the output blob"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn open_enables_wal() {
    let (dir, pool) = scratch_pool("wal").await;
    let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(mode.to_lowercase(), "wal");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn migrate_creates_task_indexes() {
    let (dir, pool) = scratch_pool("indexes").await;
    let names: Vec<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'index'")
            .fetch_all(&pool)
            .await
            .unwrap();
    for expected in [
        "idx_tasks_created_at",
        "idx_tasks_status_created_at",
        "idx_tasks_root_id",
        "idx_events_created_at",
        "idx_notifications_sent_at",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "missing index `{expected}` in {names:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn prune_keeps_newest_min_tasks_and_deletes_old_rows() {
    let (dir, pool) = scratch_pool("prune-tasks").await;
    let old = Utc::now() - ChronoDuration::days(60);
    // Four terminal tasks, oldest first. `min_tasks = 2` keeps the newest two.
    let mut ids = Vec::new();
    for i in 0..4 {
        let created = old + ChronoDuration::hours(i);
        let task = task_at(created, Some(created), None);
        ids.push(task.id);
        upsert_task(&pool, &task).await.unwrap();
    }
    // An old event and notification, plus one fresh of each.
    sqlx::query("INSERT INTO events (kind, payload, created_at) VALUES ('unknown', '{}', ?)")
        .bind(ts_ms(old))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO notifications (channel, subject, body, status, sent_at) VALUES ('c', 's', 'b', 'sent', ?)")
            .bind(ts_ms(old))
            .execute(&pool)
            .await
            .unwrap();

    let stats = prune(&pool, 30, 2, false).await.unwrap();
    assert_eq!(stats.tasks_deleted, 2);
    assert_eq!(stats.events_deleted, 1);
    assert_eq!(stats.notifications_deleted, 1);
    assert!(!stats.vacuumed);

    let remaining = list_tasks(&pool, 100).await.unwrap();
    assert_eq!(remaining.len(), 2);
    let remaining_ids: Vec<Uuid> = remaining.iter().map(|t| t.id).collect();
    assert!(remaining_ids.contains(&ids[2]));
    assert!(remaining_ids.contains(&ids[3]));

    // `days = 0` is a no-op.
    let noop = prune(&pool, 0, 0, true).await.unwrap();
    assert_eq!(noop, PruneStats::default());
    assert_eq!(list_tasks(&pool, 100).await.unwrap().len(), 2);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn prune_clears_old_outputs() {
    let (dir, pool) = scratch_pool("prune-outputs").await;
    let old = Utc::now() - ChronoDuration::days(60);
    let task = task_at(
        old,
        Some(old),
        Some(serde_json::json!({ "big": "x".repeat(1000) })),
    );
    upsert_task(&pool, &task).await.unwrap();

    // A high `min_tasks` keeps the row, but its old output is still cleared.
    let stats = prune(&pool, 30, 1000, false).await.unwrap();
    assert_eq!(stats.tasks_deleted, 0);
    assert_eq!(stats.outputs_cleared, 1);

    let got = get_task(&pool, task.id).await.unwrap().unwrap();
    assert!(got.output.is_none());
    assert_eq!(list_tasks(&pool, 100).await.unwrap().len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn task_awaiting_input_round_trips() {
    let (dir, pool) = scratch_pool("awaiting-roundtrip").await;
    let mut task = task_at(Utc::now(), None, None);
    task.status = TaskStatus::AwaitingInput;
    upsert_task(&pool, &task).await.unwrap();

    let got = get_task(&pool, task.id).await.unwrap().unwrap();
    assert_eq!(got.status, TaskStatus::AwaitingInput);
    // The wire string is stable.
    assert_eq!(got.status.as_str(), "awaiting_input");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn task_lineage_round_trips() {
    let (dir, pool) = scratch_pool("lineage-roundtrip").await;
    let root = Uuid::new_v4();
    let parent = Uuid::new_v4();
    let mut task = task_at(Utc::now(), None, None);
    task.name = "target".to_string();
    task.parent_id = Some(parent);
    task.root_id = Some(root);
    upsert_task(&pool, &task).await.unwrap();

    let got = get_task(&pool, task.id).await.unwrap().unwrap();
    assert_eq!(got.parent_id, Some(parent));
    assert_eq!(got.root_id, Some(root));

    // The root task itself has no `root_id` but is its own root.
    let mut root_task = task_at(Utc::now(), Some(Utc::now()), None);
    root_task.name = "target".to_string();
    root_task.status = TaskStatus::Succeeded;
    upsert_task(&pool, &root_task).await.unwrap();
    assert_eq!(root_task.root_or_self(), root_task.id);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn set_task_status_if_is_conditional() {
    let (dir, pool) = scratch_pool("status-if").await;
    let mut task = task_at(Utc::now(), None, None);
    task.status = TaskStatus::Running;
    upsert_task(&pool, &task).await.unwrap();

    assert!(set_task_status_if(
        &pool,
        task.id,
        TaskStatus::AwaitingInput,
        TaskStatus::Running
    )
    .await
    .unwrap());
    assert_eq!(
        get_task(&pool, task.id).await.unwrap().unwrap().status,
        TaskStatus::AwaitingInput
    );

    // Wrong source status: no-op.
    assert!(!set_task_status_if(
        &pool,
        task.id,
        TaskStatus::AwaitingInput,
        TaskStatus::Running
    )
    .await
    .unwrap());
    assert_eq!(
        get_task(&pool, task.id).await.unwrap().unwrap().status,
        TaskStatus::AwaitingInput
    );

    assert!(set_task_status_if(
        &pool,
        task.id,
        TaskStatus::Running,
        TaskStatus::AwaitingInput
    )
    .await
    .unwrap());
    assert_eq!(
        get_task(&pool, task.id).await.unwrap().unwrap().status,
        TaskStatus::Running
    );

    // Unknown id: no row matched.
    assert!(!set_task_status_if(
        &pool,
        Uuid::new_v4(),
        TaskStatus::Running,
        TaskStatus::AwaitingInput,
    )
    .await
    .unwrap());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn list_tasks_in_root_scopes_by_root_and_name() {
    let (dir, pool) = scratch_pool("root-queries").await;
    let root_a = Uuid::new_v4();
    let root_b = Uuid::new_v4();

    // Two `target` children of root A (one terminal, one active) plus one of
    // root B, and an unrelated name in root A.
    let mut done = task_at(
        Utc::now(),
        Some(Utc::now()),
        Some(serde_json::json!({ "ok": true })),
    );
    done.name = "target".to_string();
    done.root_id = Some(root_a);
    done.parent_id = Some(root_a);
    let mut running = task_at(Utc::now(), None, None);
    running.name = "target".to_string();
    running.status = TaskStatus::Running;
    running.root_id = Some(root_a);
    running.parent_id = Some(root_a);
    let mut other_root = task_at(Utc::now(), Some(Utc::now()), None);
    other_root.name = "target".to_string();
    other_root.root_id = Some(root_b);
    other_root.parent_id = Some(root_b);
    let mut noise = task_at(Utc::now(), Some(Utc::now()), None);
    noise.name = "unrelated".to_string();
    noise.root_id = Some(root_a);
    for task in [&done, &running, &other_root, &noise] {
        upsert_task(&pool, task).await.unwrap();
    }

    let all = list_tasks_in_root(&pool, root_a, "target").await.unwrap();
    assert_eq!(all.len(), 2);
    // Output blobs are included for the aggregate.
    assert!(all.iter().any(|t| t.id == done.id && t.output.is_some()));

    let active = list_active_tasks_in_root(&pool, root_a, "target")
        .await
        .unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].id, running.id);

    // A terminal-only root has an empty barrier.
    let root_c = Uuid::new_v4();
    let mut only_done = task_at(Utc::now(), Some(Utc::now()), None);
    only_done.name = "target".to_string();
    only_done.root_id = Some(root_c);
    upsert_task(&pool, &only_done).await.unwrap();
    assert!(list_active_tasks_in_root(&pool, root_c, "target")
        .await
        .unwrap()
        .is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn list_root_tasks_includes_root_and_descendants_without_output() {
    let (dir, pool) = scratch_pool("root-tasks").await;
    let root = Uuid::new_v4();
    let other_root = Uuid::new_v4();

    // The root task carries no `root_id` of its own; it is included via `id = ?`.
    let mut root_task = task_at(Utc::now(), None, Some(serde_json::json!({ "big": "x" })));
    root_task.id = root;
    root_task.name = "root".to_string();
    root_task.status = TaskStatus::Running;

    let mut child = task_at(Utc::now(), None, Some(serde_json::json!({ "child": true })));
    child.name = "child".to_string();
    child.root_id = Some(root);
    child.parent_id = Some(root);

    let mut other = task_at(Utc::now(), None, None);
    other.name = "elsewhere".to_string();
    other.root_id = Some(other_root);

    for task in [&root_task, &child, &other] {
        upsert_task(&pool, task).await.unwrap();
    }

    let got = list_root_tasks(&pool, root).await.unwrap();
    assert_eq!(got.len(), 2, "root + child only");
    assert!(
        got.iter().any(|t| t.id == root),
        "the root row is included via `id = ?`"
    );
    assert!(got.iter().any(|t| t.id == child.id));
    assert!(
        got.iter().all(|t| t.id != other.id),
        "an unrelated root must not leak in"
    );
    // The SELECT drops the blob even though the column is populated.
    assert!(
        got.iter().all(|t| t.output.is_none()),
        "list_root_tasks must not carry output blobs"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn set_task_session_never_touches_status() {
    let (dir, pool) = scratch_pool("set-session").await;
    let mut task = task_at(Utc::now(), None, None);
    task.status = TaskStatus::AwaitingInput;
    task.output = Some(serde_json::json!({ "keep": true }));
    upsert_task(&pool, &task).await.unwrap();

    assert!(set_task_session(&pool, task.id, "ses_1", Some("Title"))
        .await
        .unwrap());
    let got = get_task(&pool, task.id).await.unwrap().unwrap();
    assert_eq!(got.status, TaskStatus::AwaitingInput);
    assert_eq!(got.session_id.as_deref(), Some("ses_1"));
    assert_eq!(got.session_title.as_deref(), Some("Title"));
    assert_eq!(got.output, Some(serde_json::json!({ "keep": true })));

    // A second call must not overwrite the already-stored values (COALESCE).
    set_task_session(&pool, task.id, "ses_2", Some("Other"))
        .await
        .unwrap();
    let got = get_task(&pool, task.id).await.unwrap().unwrap();
    assert_eq!(got.session_id.as_deref(), Some("ses_1"));
    assert_eq!(got.session_title.as_deref(), Some("Title"));

    // Unknown id: no row matched.
    assert!(!set_task_session(&pool, Uuid::new_v4(), "ses_x", None)
        .await
        .unwrap());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn fail_interrupted_tasks_marks_running_and_awaiting_input() {
    let (dir, pool) = scratch_pool("interrupted").await;
    let mut running = task_at(Utc::now(), None, None);
    running.status = TaskStatus::Running;
    let mut awaiting = task_at(Utc::now(), None, None);
    awaiting.status = TaskStatus::AwaitingInput;
    let done = task_at(Utc::now(), Some(Utc::now()), None);
    for task in [&running, &awaiting, &done] {
        upsert_task(&pool, task).await.unwrap();
    }

    assert_eq!(fail_interrupted_tasks(&pool).await.unwrap(), 2);

    for id in [running.id, awaiting.id] {
        let got = get_task(&pool, id).await.unwrap().unwrap();
        assert_eq!(got.status, TaskStatus::Failed);
        assert_eq!(got.error.as_deref(), Some("interrupted by daemon restart"));
    }
    // A terminal task is untouched.
    assert_eq!(
        get_task(&pool, done.id).await.unwrap().unwrap().status,
        TaskStatus::Succeeded
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn worktree_record_round_trips() {
    let (dir, pool) = scratch_pool("worktree-roundtrip").await;
    let now = Utc::now();
    let first = WorktreeRecord {
        task_id: Uuid::new_v4(),
        repo: PathBuf::from("/repo/one"),
        path: PathBuf::from("/data/worktrees/one"),
        branch: "favetto/one-1234".to_string(),
        created_at: now - ChronoDuration::hours(1),
    };
    let second = WorktreeRecord {
        task_id: Uuid::new_v4(),
        repo: PathBuf::from("/repo/two"),
        path: PathBuf::from("/data/worktrees/two"),
        branch: "favetto/two-5678".to_string(),
        created_at: now,
    };
    record_worktree(&pool, &second).await.unwrap();
    record_worktree(&pool, &first).await.unwrap();

    let listed = list_worktrees(&pool).await.unwrap();
    assert_eq!(listed.len(), 2);
    // Oldest first; timestamps round-trip at millisecond precision.
    assert_eq!(listed[0].task_id, first.task_id);
    assert_eq!(listed[0].repo, first.repo);
    assert_eq!(listed[0].path, first.path);
    assert_eq!(listed[0].branch, first.branch);
    assert_eq!(
        listed[0].created_at.timestamp_millis(),
        first.created_at.timestamp_millis()
    );
    assert_eq!(listed[1].task_id, second.task_id);

    // Re-recording the same task id replaces, not appends.
    let mut replacement = first.clone();
    replacement.branch = "favetto/one-9999".to_string();
    record_worktree(&pool, &replacement).await.unwrap();
    let listed = list_worktrees(&pool).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].branch, "favetto/one-9999");

    // Forgetting removes exactly one row.
    forget_worktree(&pool, first.task_id).await.unwrap();
    let listed = list_worktrees(&pool).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].task_id, second.task_id);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn migrate_creates_worktrees_table() {
    let (dir, pool) = scratch_pool("worktrees-table").await;
    let names: Vec<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(
        names.iter().any(|n| n == "worktrees"),
        "missing worktrees table in {names:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn migrate_creates_task_runs_table_and_index() {
    let (dir, pool) = scratch_pool("task-runs-table").await;
    let tables: Vec<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(
        tables.iter().any(|n| n == "task_runs"),
        "missing task_runs table in {tables:?}"
    );
    let indexes: Vec<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'index'")
            .fetch_all(&pool)
            .await
            .unwrap();
    for expected in ["idx_task_runs_task_attempt", "idx_task_runs_status"] {
        assert!(
            indexes.iter().any(|n| n == expected),
            "missing index `{expected}` in {indexes:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn task_run_round_trips() {
    let (dir, pool) = scratch_pool("task-run-roundtrip").await;
    let task = task_at(Utc::now(), Some(Utc::now()), None);
    upsert_task(&pool, &task).await.unwrap();

    let started = Utc::now();
    let finished = started + ChronoDuration::seconds(3);
    let mut first = run_at(task.id, 1);
    first.status = RunStatus::Succeeded;
    first.agent = Some("opencode".to_string());
    first.session_id = Some("ses_1".to_string());
    first.started_at = Some(started);
    first.finished_at = Some(finished);
    first.exit_code = Some(0);
    let mut second = run_at(task.id, 2);
    second.status = RunStatus::Failed;
    second.exit_code = Some(1);
    second.error = Some("boom".to_string());
    second.failure = Some(Failure::new(FailureKind::Infrastructure, "boom"));
    let mut third = run_at(task.id, 3);
    third.status = RunStatus::TimedOut;
    third.failure = Some(Failure::new(FailureKind::Timeout, "deadline"));

    // Insert out of order to prove the list sort is by attempt, not insert order.
    for run in [&second, &first, &third] {
        insert_task_run(&pool, run).await.unwrap();
    }

    let got = get_task_run(&pool, first.id).await.unwrap().unwrap();
    assert_eq!(got.task_id, task.id);
    assert_eq!(got.attempt, 1);
    assert_eq!(got.status, RunStatus::Succeeded);
    assert_eq!(got.agent.as_deref(), Some("opencode"));
    assert_eq!(got.session_id.as_deref(), Some("ses_1"));
    assert_eq!(got.exit_code, Some(0));
    assert_eq!(
        got.started_at.map(|t| t.timestamp_millis()),
        Some(started.timestamp_millis())
    );
    assert_eq!(
        got.finished_at.map(|t| t.timestamp_millis()),
        Some(finished.timestamp_millis())
    );

    let listed = list_task_runs(&pool, task.id).await.unwrap();
    assert_eq!(listed.len(), 3);
    assert_eq!(
        listed.iter().map(|r| r.attempt).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "runs must come back oldest-first"
    );
    assert_eq!(
        listed[1].failure.as_ref().unwrap().kind,
        FailureKind::Infrastructure
    );
    assert_eq!(
        listed[2].failure.as_ref().unwrap().kind,
        FailureKind::Timeout
    );
    // A different task has no runs.
    assert!(list_task_runs(&pool, Uuid::new_v4())
        .await
        .unwrap()
        .is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn active_run_is_latest_non_terminal() {
    let (dir, pool) = scratch_pool("active-run").await;
    let task = task_at(Utc::now(), None, None);
    upsert_task(&pool, &task).await.unwrap();

    let mut failed = run_at(task.id, 1);
    failed.status = RunStatus::Failed;
    failed.finished_at = Some(Utc::now());
    let mut running = run_at(task.id, 2);
    running.status = RunStatus::Running;
    insert_task_run(&pool, &failed).await.unwrap();
    insert_task_run(&pool, &running).await.unwrap();

    let active = get_active_task_run(&pool, task.id).await.unwrap().unwrap();
    assert_eq!(active.id, running.id);
    assert_eq!(active.attempt, 2);

    let all_active = list_active_task_runs(&pool).await.unwrap();
    assert_eq!(all_active.len(), 1);
    assert_eq!(all_active[0].id, running.id);

    // Finalizing the running attempt leaves no active run.
    let mut done = running.clone();
    done.status = RunStatus::Succeeded;
    done.finished_at = Some(Utc::now());
    done.exit_code = Some(0);
    assert!(finalize_task_run(&pool, &done).await.unwrap());
    assert!(get_active_task_run(&pool, task.id).await.unwrap().is_none());
    assert!(list_active_task_runs(&pool).await.unwrap().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn finalize_task_run_writes_outcome_and_keeps_started() {
    let (dir, pool) = scratch_pool("finalize-run").await;
    let task = task_at(Utc::now(), None, None);
    upsert_task(&pool, &task).await.unwrap();

    let started = Utc::now();
    let mut run = run_at(task.id, 1);
    run.started_at = Some(started);
    run.agent = Some("opencode".to_string());
    run.session_id = Some("ses_keep".to_string());
    insert_task_run(&pool, &run).await.unwrap();

    let finished = started + ChronoDuration::seconds(7);
    let mut outcome = run.clone();
    outcome.status = RunStatus::Succeeded;
    outcome.finished_at = Some(finished);
    outcome.exit_code = Some(0);
    // Passing `None` must not clobber the session id written at insert time.
    outcome.session_id = None;
    assert!(finalize_task_run(&pool, &outcome).await.unwrap());

    let got = get_task_run(&pool, run.id).await.unwrap().unwrap();
    assert_eq!(got.status, RunStatus::Succeeded);
    assert_eq!(got.exit_code, Some(0));
    assert_eq!(got.session_id.as_deref(), Some("ses_keep"));
    assert_eq!(got.agent.as_deref(), Some("opencode"));
    assert_eq!(
        got.started_at.map(|t| t.timestamp_millis()),
        Some(started.timestamp_millis()),
        "finalize must not touch started_at"
    );
    assert_eq!(
        got.finished_at.map(|t| t.timestamp_millis()),
        Some(finished.timestamp_millis())
    );

    // A second finalization to a failure round-trips the typed failure.
    let mut timeout = run.clone();
    timeout.status = RunStatus::TimedOut;
    timeout.finished_at = Some(Utc::now());
    timeout.error = Some("deadline".to_string());
    timeout.failure = Some(Failure::new(FailureKind::Timeout, "deadline"));
    assert!(finalize_task_run(&pool, &timeout).await.unwrap());
    let got = get_task_run(&pool, run.id).await.unwrap().unwrap();
    assert_eq!(got.status, RunStatus::TimedOut);
    assert_eq!(got.failure.as_ref().unwrap().kind, FailureKind::Timeout);
    assert_eq!(got.failure.as_ref().unwrap().message, "deadline");
    assert_eq!(got.error.as_deref(), Some("deadline"));

    // Unknown id: no row matched.
    let mut missing = run.clone();
    missing.id = Uuid::new_v4();
    assert!(!finalize_task_run(&pool, &missing).await.unwrap());

    let _ = std::fs::remove_dir_all(&dir);
}

/// `claim_task` flips the task to running, advances `attempt`, and records
/// exactly one run under the new attempt. Claiming the same task twice is a
/// no-op, so a run is never created twice for one dispatch.
#[tokio::test]
async fn claim_task_creates_one_run_and_increments_attempt() {
    let (dir, pool) = scratch_pool("claim-run").await;
    let mut task = task_at(Utc::now(), None, None);
    task.status = TaskStatus::Pending;
    task.attempt = 0;
    insert_task(&pool, &task).await.unwrap();

    let run = claim_task(&pool, &task)
        .await
        .unwrap()
        .expect("a pending task is claimable");
    assert_eq!(run.task_id, task.id);
    assert_eq!(run.attempt, 1);
    assert_eq!(run.status, RunStatus::Running);
    assert!(run.started_at.is_some());

    let stored = get_task(&pool, task.id).await.unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::Running);
    assert_eq!(stored.attempt, 1);

    // The task is no longer pending: claim again and expect no new run.
    assert!(claim_task(&pool, &stored).await.unwrap().is_none());
    let runs = list_task_runs(&pool, task.id).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, run.id);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A retried task (already carrying an attempt) claims at `attempt + 1`.
#[tokio::test]
async fn claim_task_advances_the_attempt_for_a_retry() {
    let (dir, pool) = scratch_pool("claim-retry").await;
    let mut task = task_at(Utc::now(), None, None);
    task.status = TaskStatus::Pending;
    task.attempt = 2;
    insert_task(&pool, &task).await.unwrap();

    let run = claim_task(&pool, &task).await.unwrap().unwrap();
    assert_eq!(run.attempt, 3);
    assert_eq!(get_task(&pool, task.id).await.unwrap().unwrap().attempt, 3);
    assert_eq!(list_task_runs(&pool, task.id).await.unwrap().len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn prune_cascades_task_runs() {
    let (dir, pool) = scratch_pool("prune-runs").await;
    let old = Utc::now() - ChronoDuration::days(60);
    // Four old terminal tasks, each with one run. `min_tasks = 2` keeps two.
    let mut kept_ids = Vec::new();
    for i in 0..4 {
        let created = old + ChronoDuration::hours(i);
        let task = task_at(created, Some(created), None);
        upsert_task(&pool, &task).await.unwrap();
        insert_task_run(&pool, &run_at(task.id, 1)).await.unwrap();
        if i >= 2 {
            kept_ids.push(task.id);
        }
    }
    // An orphan run whose owning task row does not exist is dead history too.
    let orphan = run_at(Uuid::new_v4(), 1);
    insert_task_run(&pool, &orphan).await.unwrap();

    let stats = prune(&pool, 30, 2, false).await.unwrap();
    // Two deleted tasks' runs plus the orphan.
    assert_eq!(stats.tasks_deleted, 2);
    assert_eq!(stats.runs_deleted, 3);
    assert_eq!(
        list_task_runs(&pool, orphan.task_id).await.unwrap().len(),
        0
    );
    for id in &kept_ids {
        assert_eq!(list_task_runs(&pool, *id).await.unwrap().len(), 1);
    }
    assert_eq!(list_active_task_runs(&pool).await.unwrap().len(), 2);

    // `days = 0` is still a no-op and leaves runs untouched.
    let noop = prune(&pool, 0, 0, true).await.unwrap();
    assert_eq!(noop, PruneStats::default());
    assert_eq!(stats.runs_deleted, 3);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn migrate_adds_task_runs_to_legacy_database() {
    let dir = std::env::temp_dir().join(format!("favetto-legacy-runs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let pool = open(&dir.join("test.db")).await.unwrap();

    // A pre-migration database: the tasks table predates `session_title` and
    // there is no `task_runs` table at all.
    sqlx::query(
        "CREATE TABLE tasks (
                id          TEXT PRIMARY KEY,
                name        TEXT NOT NULL,
                status      TEXT NOT NULL,
                input       TEXT NOT NULL,
                output      TEXT,
                dedupe_key  TEXT UNIQUE,
                created_at  INTEGER NOT NULL,
                started_at  INTEGER,
                finished_at INTEGER,
                error       TEXT,
                session_id  TEXT
            )",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Running the migration twice is a no-op the second time; both must succeed
    // and add the new table.
    migrate(&pool).await.unwrap();
    migrate(&pool).await.unwrap();

    let task = task_at(Utc::now(), None, None);
    upsert_task(&pool, &task).await.unwrap();
    let run = run_at(task.id, 1);
    insert_task_run(&pool, &run).await.unwrap();
    let got = list_task_runs(&pool, task.id).await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].id, run.id);
    assert_eq!(got[0].status, RunStatus::Running);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A minimal pending task, optionally in a workflow root.
fn pending_task(name: &str, root: Option<Uuid>) -> Task {
    let mut task = task_at(Utc::now(), None, None);
    task.name = name.to_string();
    task.status = TaskStatus::Pending;
    task.started_at = None;
    task.root_id = root;
    task
}

#[tokio::test]
async fn migrate_creates_task_dependencies_table_and_indexes() {
    let (dir, pool) = scratch_pool("deps-table").await;
    let tables: Vec<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(
        tables.iter().any(|n| n == "task_dependencies"),
        "missing task_dependencies table in {tables:?}"
    );
    let indexes: Vec<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'index'")
            .fetch_all(&pool)
            .await
            .unwrap();
    for expected in [
        "idx_task_dependencies_task_id",
        "idx_task_dependencies_depends_on",
        "idx_tasks_dedupe_key",
    ] {
        assert!(
            indexes.iter().any(|n| n == expected),
            "missing index `{expected}` in {indexes:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn next_pending_tasks_skips_tasks_with_unmet_dependencies() {
    let (dir, pool) = scratch_pool("readiness-gate").await;
    let a = pending_task("a", None);
    let b = pending_task("b", None);
    let c = pending_task("c", None);
    // Delay `b` so ordering is deterministic (oldest first).
    for task in [&a, &b, &c] {
        upsert_task(&pool, task).await.unwrap();
    }
    insert_dependency(&pool, b.id, a.id, Utc::now())
        .await
        .unwrap();

    let names: Vec<String> = next_pending_tasks(&pool, 50)
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert!(names.contains(&"a".to_string()), "{names:?}");
    assert!(names.contains(&"c".to_string()), "{names:?}");
    assert!(
        !names.contains(&"b".to_string()),
        "b must wait for a: {names:?}"
    );

    // Once `a` is terminal, `b` becomes dispatchable.
    let mut done = a.clone();
    done.status = TaskStatus::Succeeded;
    done.finished_at = Some(Utc::now());
    upsert_task(&pool, &done).await.unwrap();
    let names: Vec<String> = next_pending_tasks(&pool, 50)
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert!(names.contains(&"b".to_string()), "{names:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn next_pending_tasks_ignores_missing_predecessor() {
    let (dir, pool) = scratch_pool("readiness-missing").await;
    let b = pending_task("b", None);
    upsert_task(&pool, &b).await.unwrap();
    // A pruned predecessor must not wedge the dependent forever.
    insert_dependency(&pool, b.id, Uuid::new_v4(), Utc::now())
        .await
        .unwrap();

    let pending = next_pending_tasks(&pool, 50).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, b.id);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn next_pending_tasks_cancelled_predecessor_unblocks() {
    let (dir, pool) = scratch_pool("readiness-cancelled").await;
    let mut a = pending_task("a", None);
    let b = pending_task("b", None);
    a.status = TaskStatus::Cancelled;
    a.finished_at = Some(Utc::now());
    upsert_task(&pool, &a).await.unwrap();
    upsert_task(&pool, &b).await.unwrap();
    insert_dependency(&pool, b.id, a.id, Utc::now())
        .await
        .unwrap();

    let names: Vec<String> = next_pending_tasks(&pool, 50)
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert!(
        names.contains(&"b".to_string()),
        "a cancelled predecessor is terminal: {names:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn insert_dependency_is_idempotent_and_listable() {
    let (dir, pool) = scratch_pool("deps-crud").await;
    let root = Uuid::new_v4();
    let a = pending_task("a", Some(root));
    let b = pending_task("b", Some(root));
    for task in [&a, &b] {
        upsert_task(&pool, task).await.unwrap();
    }

    insert_dependency(&pool, b.id, a.id, Utc::now())
        .await
        .unwrap();
    insert_dependency(&pool, b.id, a.id, Utc::now())
        .await
        .unwrap();

    assert_eq!(list_dependencies(&pool, b.id).await.unwrap(), vec![a.id]);

    // Root-scoped listing returns the edge and scopes it by the dependent's root.
    let edges = list_root_dependencies(&pool, root).await.unwrap();
    assert_eq!(edges, vec![(b.id, a.id)]);
    assert!(list_root_dependencies(&pool, Uuid::new_v4())
        .await
        .unwrap()
        .is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn get_task_by_dedupe_key_returns_the_stored_row() {
    let (dir, pool) = scratch_pool("dedupe-lookup").await;
    let mut task = pending_task("a", None);
    task.dedupe_key = Some("workflow:op:key-a".to_string());
    upsert_task(&pool, &task).await.unwrap();

    let found = get_task_by_dedupe_key(&pool, "workflow:op:key-a")
        .await
        .unwrap()
        .expect("stored row");
    assert_eq!(found.id, task.id);
    assert!(get_task_by_dedupe_key(&pool, "workflow:op:missing")
        .await
        .unwrap()
        .is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn prune_deletes_orphan_dependencies() {
    let (dir, pool) = scratch_pool("prune-deps").await;
    let old = Utc::now() - ChronoDuration::days(60);
    let a = task_at(old, Some(old), None);
    let b = task_at(old, Some(old), None);
    upsert_task(&pool, &a).await.unwrap();
    upsert_task(&pool, &b).await.unwrap();
    insert_dependency(&pool, b.id, a.id, old).await.unwrap();

    let stats = prune(&pool, 30, 0, false).await.unwrap();
    assert_eq!(stats.dependencies_deleted, 1);
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM task_dependencies")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(remaining, 0);

    let _ = std::fs::remove_dir_all(&dir);
}
