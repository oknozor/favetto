use super::*;
use chrono::Utc;
use favetto_core::model::{Event, Failure, FailureKind};

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
