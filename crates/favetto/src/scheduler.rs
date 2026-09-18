//! Cron scheduler: loads `schedules` from SQLite and runs them via
//! `tokio-cron-scheduler`. Each fire enqueues a task and emits a `CronTick` event.
//! Schedules can be added/removed live through the RPC API, and catalog tasks with
//! a `schedule` header are registered here as recurring tasks.

use std::sync::Arc;

use chrono::Utc;
use tokio_cron_scheduler::{Job, JobScheduler};
use uuid::Uuid;

use favetto_core::model::Schedule;

use crate::db;
use crate::executor;
use crate::state::State;

/// Register a schedule's job with the scheduler. Returns the job id.
pub async fn register(scheduler: &JobScheduler, state: Arc<State>, schedule: &Schedule) -> anyhow::Result<Uuid> {
    let cron = schedule.cron.clone();
    let state2 = state.clone();
    let task = schedule.task.clone();
    let input = schedule.input.clone();
    let schedule_id = schedule.id.clone();

    let job = Job::new_async(cron, move |_uuid, _sched| {
        let state = state2.clone();
        let task = task.clone();
        let input = input.clone();
        let schedule_id = schedule_id.clone();
        Box::pin(async move {
            let dedupe = format!("schedule:{schedule_id}:{}", Utc::now().timestamp());
            if let Ok(task) = executor::enqueue_task(&state, task, input, Some(dedupe)).await {
                state
                    .emit_event(
                        favetto_core::model::EventKind::CronTick,
                        serde_json::json!({ "schedule_id": schedule_id, "task_id": task.id }),
                    )
                    .await;
            }
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

/// Insert-or-update a schedule, (re)registering its cron job. Shared by the RPC
/// handler and catalog schedule sync.
pub async fn upsert(state: &Arc<State>, schedule: &Schedule) -> anyhow::Result<()> {
    if let Some(old_job_id) = db::get_schedule_job_id(&state.db, &schedule.id).await? {
        if let Ok(job_uuid) = Uuid::parse_str(&old_job_id) {
            let _ = unregister(&state.scheduler, &job_uuid).await;
        }
    }
    db::upsert_schedule(&state.db, schedule).await?;
    if schedule.enabled {
        let job_id = register(&state.scheduler, state.clone(), schedule).await?;
        db::set_schedule_job_id(&state.db, &schedule.id, &job_id.to_string()).await?;
    }
    Ok(())
}

/// Register recurring catalog tasks (those with a `schedule` header) as schedules.
pub async fn sync_catalog_schedules(state: &Arc<State>) -> anyhow::Result<()> {
    let catalog = state.catalog.read().unwrap().clone();
    for def in catalog {
        let Some(cron) = &def.schedule else {
            continue;
        };
        let schedule = Schedule {
            id: format!("catalog:{}", def.name),
            cron: cron.clone(),
            task: def.name.clone(),
            input: serde_json::json!({}),
            enabled: true,
            last_run: None,
        };
        if let Err(e) = upsert(state, &schedule).await {
            tracing::warn!(error = %e, task = %def.name, "failed to register catalog schedule");
        }
    }
    Ok(())
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
