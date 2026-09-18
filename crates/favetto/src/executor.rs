//! Task executor: drains the pending task queue and runs each task through the
//! agent runtime, plus reacts to `TaskFinished` events to start tasks that depend
//! on them (`needs = "other:finished"`).

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use uuid::Uuid;

use favetto_core::model::{Event, EventKind, Task, TaskStatus};

use crate::db;
use crate::event_bus::ServerPush;
use crate::state::State;
use crate::runtime;

/// Spawn the queue poller and the dependency listener.
pub fn spawn(state: Arc<State>) -> tokio::task::JoinHandle<()> {
    let poller_state = state.clone();
    let poller = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        loop {
            interval.tick().await;
            match db::next_pending_task(&poller_state.db).await {
                Ok(Some(task)) => {
                    tracing::info!(task_id = %task.id, task = %task.name, "executing task");
                    run_one(&poller_state, task).await;
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, "failed to poll task queue"),
            }
        }
    });

    let dep_state = state.clone();
    tokio::spawn(async move {
        let mut rx = dep_state.bus.subscribe();
        loop {
            match rx.recv().await {
                Ok(ServerPush::Event(ev)) => {
                    if ev.kind == EventKind::TaskFinished {
                        start_dependents(&dep_state, &ev).await;
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(n, "executor dependency listener lagged");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    poller
}

/// Enqueue a task (idle), announcing it on the bus and emitting `TaskIdle`.
pub async fn enqueue_task(
    state: &State,
    name: String,
    input: serde_json::Value,
    dedupe_key: Option<String>,
) -> anyhow::Result<Task> {
    let task = Task {
        id: Uuid::new_v4(),
        name: name.clone(),
        status: TaskStatus::Pending,
        input,
        output: None,
        dedupe_key,
        created_at: Utc::now(),
        started_at: None,
        finished_at: None,
        error: None,
    };
    db::insert_task(&state.db, &task).await?;
    crate::metrics::inc_tasks();
    state.bus.publish(ServerPush::TaskUpdated(task.clone()));
    state
        .emit_event(
            EventKind::TaskIdle,
            serde_json::json!({ "name": name, "task_id": task.id }),
        )
        .await;
    Ok(task)
}

async fn run_one(state: &Arc<State>, task: Task) {
    let mut task = task;
    task.status = TaskStatus::Running;
    task.started_at = Some(Utc::now());
    let _ = db::upsert_task(&state.db, &task).await;
    state.bus.publish(ServerPush::TaskUpdated(task.clone()));
    state
        .emit_event(
            EventKind::TaskStarted,
            serde_json::json!({ "name": task.name, "task_id": task.id }),
        )
        .await;

    let config = state.config.read().unwrap().clone();

    // Look up the task's catalog definition (which carries its model + prompt).
    let def = state
        .catalog
        .read()
        .unwrap()
        .iter()
        .find(|d| d.name == task.name)
        .cloned();

    let outcome = match def {
        Some(def) => runtime::run_task(&def, &config, task.input.clone()).await,
        None => Err(anyhow::anyhow!("task '{}' not found in the catalog", task.name)),
    };

    let success = outcome.is_ok();
    match outcome {
        Ok(output) => {
            task.status = TaskStatus::Succeeded;
            task.output = Some(output);
            task.error = None;
        }
        Err(e) => {
            task.status = TaskStatus::Failed;
            task.error = Some(e.to_string());
        }
    }
    task.finished_at = Some(Utc::now());

    let _ = db::upsert_task(&state.db, &task).await;
    state.bus.publish(ServerPush::TaskUpdated(task.clone()));

    let kind = if success {
        EventKind::TaskCompleted
    } else {
        EventKind::TaskFailed
    };
    state
        .emit_event(kind, serde_json::json!({ "task_id": task.id }))
        .await;
    state
        .emit_event(
            EventKind::TaskFinished,
            serde_json::json!({ "name": task.name, "task_id": task.id, "success": success }),
        )
        .await;
}

/// Start every catalog task whose `needs` matches `name:finished`.
async fn start_dependents(state: &Arc<State>, ev: &Event) {
    let Some(name) = ev.payload.get("name").and_then(|n| n.as_str()) else {
        return;
    };
    let dependents: Vec<String> = state
        .catalog
        .read()
        .unwrap()
        .iter()
        .filter(|d| d.needs.as_deref() == Some(&format!("{name}:finished")))
        .map(|d| d.name.clone())
        .collect();

    for dep in dependents {
        tracing::info!(task = %dep, trigger = %name, "auto-starting dependent task");
        let dedupe = format!("needs:{}:{}", dep, ev.id);
        if let Err(e) = enqueue_task(state, dep, serde_json::json!({}), Some(dedupe)).await {
            tracing::warn!(error = %e, "failed to enqueue dependent task");
        }
    }
}
