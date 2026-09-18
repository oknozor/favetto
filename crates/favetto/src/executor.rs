//! Task executor: drains the pending task queue and runs each task through the
//! agent runtime. This closes the loop opened in M3 — hooks and schedules enqueue
//! tasks, and here they actually execute.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use favetto_core::model::{EventKind, Task, TaskStatus};

use crate::db;
use crate::event_bus::ServerPush;
use crate::state::State;
use crate::{runtime, skills};

/// Spawn a background worker that polls the queue and runs pending tasks.
pub fn spawn(state: Arc<State>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        loop {
            interval.tick().await;
            match db::next_pending_task(&state.db).await {
                Ok(Some(task)) => {
                    tracing::info!(task_id = %task.id, skill = %task.skill, "executing task");
                    run_one(&state, task).await;
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, "failed to poll task queue"),
            }
        }
    })
}

async fn run_one(state: &Arc<State>, task: Task) {
    // Mark running.
    let mut task = task;
    task.status = TaskStatus::Running;
    task.started_at = Some(Utc::now());
    let _ = db::upsert_task(&state.db, &task).await;
    state.bus.publish(ServerPush::TaskUpdated(task.clone()));

    // Load the skill, run it, and record the outcome.
    let skills = skills::load_skills(&state.skills_dir).unwrap_or_default();
    let skill = skills.into_iter().find(|s| s.name == task.skill);

    let outcome = match skill {
        Some(skill) => runtime::run_skill(&skill, &state.config, task.input.clone()).await,
        None => Err(anyhow::anyhow!("skill '{}' not found", task.skill)),
    };

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

    let kind = if task.status == TaskStatus::Succeeded {
        EventKind::TaskCompleted
    } else {
        EventKind::TaskFailed
    };
    state
        .emit_event(kind, serde_json::json!({ "task_id": task.id }))
        .await;
}
