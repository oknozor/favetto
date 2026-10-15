//! Cron scheduler: loads `schedules` from SQLite and runs them via
//! `tokio-cron-scheduler`. Each fire enqueues a task and emits a `CronTick` event.
//! Schedules can be added/removed live through the RPC API, and catalog tasks with
//! a `schedule` header are registered here as recurring tasks.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio_cron_scheduler::{Job, JobScheduler};
use uuid::Uuid;

use favetto_core::model::Schedule;

use crate::db;
use crate::executor;
use crate::state::State;

/// Compute the next UTC fire time for `cron`, using the same parser
/// configuration the scheduler registers jobs with (`croner`, seconds required,
/// day-of-month and day-of-week combined).
///
/// Returns `None` when the expression does not parse under that parser. The
/// result is strictly after `after`, so a fire exactly at `after` is skipped.
pub fn next_occurrence(cron: &str, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let schedule = croner::parser::CronParser::builder()
        .seconds(croner::parser::Seconds::Required)
        .dom_and_dow(true)
        .build()
        .parse(cron)
        .ok()?;
    schedule.find_next_occurrence(&after, false).ok()
}

/// Populate `schedule.next_run` for an enabled schedule whose cron parses.
///
/// Disabled schedules have no upcoming run, so their `next_run` stays `None`.
pub fn with_next_run(mut schedule: Schedule, now: DateTime<Utc>) -> Schedule {
    schedule.next_run = if schedule.enabled {
        next_occurrence(&schedule.cron, now)
    } else {
        None
    };
    schedule
}

/// Register a schedule's job with the scheduler. Returns the job id.
pub async fn register(
    scheduler: &JobScheduler,
    state: Arc<State>,
    schedule: &Schedule,
) -> anyhow::Result<Uuid> {
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
            if let Ok(task) = executor::enqueue_task(&state, task, input, Some(dedupe), false).await
            {
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

/// Remove a schedule: unregister its cron job and delete its row. Shared by the
/// RPC handler and catalog schedule reconciliation.
pub async fn remove(state: &Arc<State>, id: &str) -> anyhow::Result<()> {
    if let Some(job_id) = db::get_schedule_job_id(&state.db, id).await? {
        if let Ok(job_uuid) = Uuid::parse_str(&job_id) {
            let _ = unregister(&state.scheduler, &job_uuid).await;
        }
    }
    db::delete_schedule(&state.db, id).await?;
    Ok(())
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

/// Reconcile the scheduler with the catalog's recurring tasks (those with a
/// `schedule` header): register new or changed `catalog:<name>` schedules and
/// drop schedules whose task vanished or lost its `schedule` header.
pub async fn reconcile_catalog_schedules(state: &Arc<State>) -> anyhow::Result<()> {
    let catalog = state.catalog.read().clone();

    // Desired schedules, derived from the current catalog.
    let mut desired: Vec<Schedule> = Vec::new();
    for def in &catalog {
        let Some(cron) = &def.schedule else {
            continue;
        };
        desired.push(Schedule {
            id: format!("catalog:{}", def.name),
            cron: cron.clone(),
            task: def.name.clone(),
            input: serde_json::json!({}),
            enabled: true,
            last_run: None,
            next_run: None,
        });
    }

    let existing = db::list_schedules(&state.db).await?;

    // Drop catalog schedules that no longer correspond to a catalog task.
    for schedule in &existing {
        if schedule.id.starts_with("catalog:") && !desired.iter().any(|d| d.id == schedule.id) {
            if let Err(e) = remove(state, &schedule.id).await {
                tracing::warn!(error = %e, schedule = %schedule.id, "failed to remove stale catalog schedule");
            }
        }
    }

    // Upsert the desired schedules, skipping ones that are already registered
    // with the same cron so a reload doesn't re-create unchanged jobs.
    for mut schedule in desired {
        if let Some(old) = existing.iter().find(|s| s.id == schedule.id) {
            if old.cron == schedule.cron && old.task == schedule.task && old.enabled {
                continue;
            }
            schedule.last_run = old.last_run;
        }
        if let Err(e) = upsert(state, &schedule).await {
            tracing::warn!(error = %e, task = %schedule.task, "failed to register catalog schedule");
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
                if let Err(e) =
                    db::set_schedule_job_id(&state.db, &schedule.id, &job_id.to_string()).await
                {
                    tracing::warn!(error = %e, schedule = %schedule.id, "failed to persist job id");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, schedule = %schedule.id, "failed to register schedule")
            }
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

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;

    use super::*;

    /// A fixed reference instant: 2026-01-01T07:30:00Z.
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 7, 30, 0).unwrap()
    }

    fn ts(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    #[test]
    fn next_occurrence_uses_seconds_first_fields() {
        // `0 0 8 * * *` is second=0 minute=0 hour=8: the next 08:00:00.
        assert_eq!(
            next_occurrence("0 0 8 * * *", now()),
            Some(ts(2026, 1, 1, 8, 0, 0))
        );
        // A 30-second offset is honoured.
        assert_eq!(
            next_occurrence("30 0 8 * * *", now()),
            Some(ts(2026, 1, 1, 8, 0, 30))
        );
        // `0 * * * * *` fires at the top of every minute.
        assert_eq!(
            next_occurrence("0 * * * * *", now()),
            Some(ts(2026, 1, 1, 7, 31, 0))
        );
    }

    #[test]
    fn next_occurrence_rolls_over_to_the_next_day() {
        let after = ts(2026, 1, 1, 8, 0, 0);
        assert_eq!(
            next_occurrence("0 0 8 * * *", after),
            Some(ts(2026, 1, 2, 8, 0, 0))
        );
    }

    #[test]
    fn next_occurrence_rejects_expressions_the_scheduler_rejects() {
        // The scheduler requires an explicit seconds field, so a five-field
        // expression must not silently produce a next run.
        assert_eq!(next_occurrence("0 8 * * *", now()), None);
        assert_eq!(next_occurrence("not a cron", now()), None);
        assert_eq!(next_occurrence("", now()), None);
    }

    #[test]
    fn with_next_run_skips_disabled_schedules() {
        let enabled = Schedule {
            id: "enabled".to_string(),
            cron: "0 0 8 * * *".to_string(),
            task: "t".to_string(),
            input: serde_json::json!({}),
            enabled: true,
            last_run: None,
            next_run: None,
        };
        assert_eq!(
            with_next_run(enabled.clone(), now()).next_run,
            Some(ts(2026, 1, 1, 8, 0, 0))
        );

        let disabled = Schedule {
            enabled: false,
            ..enabled
        };
        assert_eq!(with_next_run(disabled, now()).next_run, None);
    }
}
