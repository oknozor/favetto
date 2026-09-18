//! Cron scheduler: loads `schedules` from SQLite and runs them via
//! `tokio-cron-scheduler`. Each fire enqueues a `run_skill` task and emits a
//! `CronTick` event. Schedules can be added/removed live through the RPC API.

use std::sync::Arc;

use chrono::Utc;
use tokio_cron_scheduler::{Job, JobScheduler};
use uuid::Uuid;

use favetto_core::model::{Schedule, Task, TaskStatus};

use crate::db;
use crate::event_bus::ServerPush;
use crate::state::State;
/// Register a schedule's job with the scheduler. Returns the job id.
pub async fn register(scheduler: &JobScheduler, state: Arc<State>, schedule: &Schedule) -> anyhow::Result<Uuid> {
    let cron = schedule.cron.clone();
    let state2 = state.clone();
    let skill = schedule.skill.clone();
    let input = schedule.input.clone();
    let schedule_id = schedule.id.clone();

    let job = Job::new_async(cron, move |_uuid, _sched| {
        let state = state2.clone();
        let skill = skill.clone();
        let input = input.clone();
        let schedule_id = schedule_id.clone();
        Box::pin(async move {
            let task = Task {
                id: Uuid::new_v4(),
                skill: skill.clone(),
                status: TaskStatus::Pending,
                input,
                output: None,
                dedupe_key: Some(format!("schedule:{schedule_id}:{}", Utc::now().timestamp())),
                created_at: Utc::now(),
                started_at: None,
                finished_at: None,
                error: None,
            };
            let _ = db::insert_task(&state.db, &task).await;
            state.bus.publish(ServerPush::TaskUpdated(task.clone()));
            state
                .emit_event(
                    favetto_core::model::EventKind::CronTick,
                    serde_json::json!({ "schedule_id": schedule_id, "task_id": task.id }),
                )
                .await;
            let _ = db::touch_schedule(&state.db, &schedule_id).await;
        })
    })?;

    let id = scheduler.add(job).await?;
    Ok(id)
}

/// Unregister a schedule's job.
pub async fn unregister(scheduler: &JobScheduler, job_id: &Uuid) -> anyhow::Result<()> {
    Ok(scheduler.remove(job_id).await?)
}

/// Load persisted schedules, register their jobs, and start the scheduler tick loop.
pub async fn start(state: &Arc<State>) -> anyhow::Result<()> {
    let schedules = db::list_schedules(&state.db).await?;
    for schedule in &schedules {
        if !schedule.enabled {
            continue;
        }
        match register(&state.scheduler, state.clone(), schedule).await {
            Ok(job_id) => {
                if let Err(e) = db::set_schedule_job_id(&state.db, &schedule.id, &job_id.to_string()).await {
                    tracing::warn!(error = %e, schedule = %schedule.id, "failed to persist job id");
                }
            }
            Err(e) => tracing::warn!(error = %e, schedule = %schedule.id, "failed to register schedule"),
        }
    }

    let runner = state.scheduler.clone();
    tokio::spawn(async move {
        if let Err(e) = runner.start().await {
            tracing::error!(error = %e, "scheduler failed");
        }
    });

    Ok(())
}
