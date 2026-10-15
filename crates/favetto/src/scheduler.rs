//! Cron scheduler: loads `schedules` from SQLite and runs them via
//! `tokio-cron-scheduler`. Each fire enqueues a task and emits a `CronTick` event.
//! Schedules can be added/removed live through the RPC API, and catalog tasks with
//! a `schedule` header are registered here as recurring tasks.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use croner::parser::{CronParser, Seconds};
use tokio_cron_scheduler::{Job, JobScheduler};
use uuid::Uuid;

use favetto_core::model::Schedule;

use crate::db;
use crate::executor;
use crate::state::State;

/// Parse a cron expression exactly like `tokio-cron-scheduler` does (seconds
/// required, day-of-month AND day-of-week) and return the next occurrence
/// strictly after `now`, evaluated in UTC. `None` for a malformed expression or
/// one with no future occurrence.
pub fn next_occurrence(cron: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let parsed = CronParser::builder()
        .seconds(Seconds::Required)
        .dom_and_dow(true)
        .build()
        .parse(cron)
        .ok()?;
    parsed.find_next_occurrence(&now, false).ok()
}

/// Fill in the derived `next_run` for each schedule: enabled schedules get the
/// next UTC occurrence, disabled schedules (and unparseable crons) get `None`.
pub fn with_next_runs(schedules: &mut [Schedule], now: DateTime<Utc>) {
    for schedule in schedules {
        schedule.next_run = if schedule.enabled {
            next_occurrence(&schedule.cron, now)
        } else {
            None
        };
    }
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
    use super::*;
    use chrono::TimeZone as _;

    /// A fixed UTC instant builder for deterministic occurrence tests.
    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).single().unwrap()
    }

    fn schedule(id: &str, cron: &str, enabled: bool) -> Schedule {
        Schedule {
            id: id.to_string(),
            cron: cron.to_string(),
            task: "t".to_string(),
            input: serde_json::Value::Null,
            enabled,
            last_run: None,
            next_run: None,
        }
    }

    #[test]
    fn next_occurrence_evaluates_the_cron_in_utc() {
        // 07:30 UTC: the daily 08:00 job (seconds-first) fires at 08:00 UTC.
        let now = at(2026, 6, 15, 7, 30, 0);
        assert_eq!(
            next_occurrence("0 0 8 * * *", now),
            Some(at(2026, 6, 15, 8, 0, 0))
        );
    }

    #[test]
    fn next_occurrence_honours_second_granularity() {
        let now = at(2026, 6, 15, 0, 0, 10);
        assert_eq!(
            next_occurrence("*/30 * * * * *", now),
            Some(at(2026, 6, 15, 0, 0, 30))
        );
    }

    #[test]
    fn next_occurrence_is_none_for_bad_or_secondless_cron() {
        let now = at(2026, 6, 15, 0, 0, 0);
        // The daemon's parser requires a seconds field, matching
        // `tokio-cron-scheduler`; a 5-field expression never schedules.
        assert_eq!(next_occurrence("0 8 * * *", now), None);
        assert_eq!(next_occurrence("not a cron", now), None);
    }

    #[test]
    fn with_next_runs_sets_enabled_and_clears_disabled() {
        let now = at(2026, 6, 15, 7, 30, 0);
        let mut schedules = vec![
            schedule("on", "0 0 8 * * *", true),
            schedule("off", "0 0 8 * * *", false),
        ];
        with_next_runs(&mut schedules, now);
        assert_eq!(schedules[0].next_run, Some(at(2026, 6, 15, 8, 0, 0)));
        assert_eq!(schedules[1].next_run, None);
    }
}
