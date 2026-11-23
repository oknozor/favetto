//! Cron scheduler: loads `schedules` from SQLite and runs them on a tokio timer.
//! Each fire enqueues a task and emits a `CronTick` event. Schedules can be
//! added/removed live through the RPC API, and catalog tasks with a `schedule`
//! header are registered here as recurring tasks.
//!
//! Jobs are driven by [`favetto_cron`] and plain [`tokio::time::sleep`], so the
//! daemon no longer depends on `croner`/`tokio-cron-scheduler` (both of which
//! require the unmaintained `chrono` crate).

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use favetto_core::model::Schedule;
use favetto_core::Timestamp;
use parking_lot::Mutex;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::db;
use crate::executor;
use crate::state::State;

/// The next UTC occurrence of `cron` strictly after `now`, or `None` when the
/// expression does not parse / has no future occurrence.
///
/// Uses the same parser configuration the daemon applies when it registers a
/// job (seconds required, day-of-month + day-of-week AND, UTC), so a shown
/// `next_run` always matches when the job would actually fire.
pub fn next_run_after(cron: &str, now: Timestamp) -> Option<Timestamp> {
    favetto_cron::next_run_after(cron, now)
}

/// A set of live cron jobs, each running on its own tokio task.
///
/// `register` adds a job immediately; there is no separate tick to start.
/// Removing a job aborts its task, which is safe even if it is mid-fire.
#[derive(Debug, Clone, Default)]
pub struct Scheduler {
    jobs: Arc<Mutex<HashMap<Uuid, JoinHandle<()>>>>,
}

impl Scheduler {
    /// Create an empty scheduler.
    pub async fn new() -> anyhow::Result<Self> {
        Ok(Self::default())
    }

    /// Validate `cron` and spawn `run` on every occurrence. Returns the job id.
    pub async fn add<F, Fut>(&self, cron: &str, run: F) -> anyhow::Result<Uuid>
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let schedule = favetto_cron::Cron::parse(cron)
            .map_err(|error| anyhow::anyhow!("invalid cron expression '{cron}': {error}"))?;
        let id = Uuid::new_v4();
        let handle = tokio::spawn(run_loop(schedule, run));
        self.jobs.lock().insert(id, handle);
        Ok(id)
    }

    /// Abort a registered job, if it exists.
    pub async fn remove(&self, id: &Uuid) -> anyhow::Result<()> {
        if let Some(handle) = self.jobs.lock().remove(id) {
            handle.abort();
        }
        Ok(())
    }
}

async fn run_loop<F, Fut>(schedule: favetto_cron::Cron, run: F)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    loop {
        let now = Timestamp::now();
        let Some(next) = schedule.next_after(now) else {
            break;
        };
        let delay = next - now;
        let nanos = u64::try_from(delay.whole_nanoseconds()).unwrap_or(u64::MAX);
        tokio::time::sleep(std::time::Duration::from_nanos(nanos)).await;
        run().await;
    }
}

/// Register a schedule's job with the scheduler. Returns the job id.
pub async fn register(
    scheduler: &Scheduler,
    state: Arc<State>,
    schedule: &Schedule,
) -> anyhow::Result<Uuid> {
    let cron = schedule.cron.clone();
    let state2 = state.clone();
    let task = schedule.task.clone();
    let input = schedule.input.clone();
    let schedule_id = schedule.id.clone();

    let id = scheduler
        .add(&cron, move || {
            let state = state2.clone();
            let task = task.clone();
            let input = input.clone();
            let schedule_id = schedule_id.clone();
            async move {
                let dedupe = format!(
                    "schedule:{schedule_id}:{}",
                    Timestamp::now().unix_timestamp()
                );
                if let Ok(task) =
                    executor::enqueue_task(&state, task, input, Some(dedupe), false).await
                {
                    state
                        .emit_event(
                            favetto_core::model::EventKind::CronTick,
                            serde_json::json!({ "schedule_id": schedule_id, "task_id": task.id }),
                        )
                        .await;
                }
                let _ = db::touch_schedule(&state.db, &schedule_id).await;
            }
        })
        .await?;
    Ok(id)
}

/// Unregister a schedule's job.
pub async fn unregister(scheduler: &Scheduler, job_id: &Uuid) -> anyhow::Result<()> {
    scheduler.remove(job_id).await
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
    } else {
        db::clear_schedule_job_id(&state.db, &schedule.id).await?;
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
    // with the same cron so a reload doesn't re-create unchanged jobs. Catalog
    // schedules are user-toggleable: `enabled` and `last_run` belong to the
    // persisted row, not the catalog header, so a paused schedule stays paused
    // across reloads and header edits.
    for mut schedule in desired {
        if let Some(old) = existing.iter().find(|s| s.id == schedule.id) {
            schedule.enabled = old.enabled;
            schedule.last_run = old.last_run;
            if old.cron == schedule.cron && old.task == schedule.task {
                continue;
            }
        }
        if let Err(e) = upsert(state, &schedule).await {
            tracing::warn!(error = %e, task = %schedule.task, "failed to register catalog schedule");
        }
    }
    Ok(())
}

/// Load persisted schedules and register their jobs.
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn next_run_after_handles_second_and_minute_forms() {
        let now = utc("2026-01-05T00:00:00Z");
        assert_eq!(
            next_run_after("0 * * * * *", now),
            Some(utc("2026-01-05T00:01:00Z"))
        );
        assert_eq!(
            next_run_after("30 0 * * * *", now),
            Some(utc("2026-01-05T00:00:30Z"))
        );
    }

    #[test]
    fn next_run_after_handles_daily_and_weekly_forms() {
        let now = utc("2026-01-05T00:00:00Z");
        assert_eq!(
            next_run_after("0 0 8 * * *", now),
            Some(utc("2026-01-05T08:00:00Z"))
        );
        // 2026-01-05 is a Monday.
        assert_eq!(
            next_run_after("0 30 9 * * 1", now),
            Some(utc("2026-01-05T09:30:00Z"))
        );
    }

    #[test]
    fn next_run_after_rejects_unparseable_expressions() {
        let now = utc("2026-01-05T00:00:00Z");
        assert_eq!(next_run_after("not a cron", now), None);
        // Five-field expressions fail the seconds-required parser, matching
        // registration.
        assert_eq!(next_run_after("0 8 * * *", now), None);
    }

    #[tokio::test]
    async fn scheduler_runs_a_job_and_can_be_removed() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let scheduler = Scheduler::new().await.unwrap();
        let fired = Arc::new(AtomicUsize::new(0));
        let fired2 = fired.clone();
        // Every second.
        let id = scheduler
            .add("* * * * * *", move || {
                let fired = fired2.clone();
                async move {
                    fired.fetch_add(1, Ordering::SeqCst);
                }
            })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while fired.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("job should fire");
        scheduler.remove(&id).await.unwrap();
        let count = fired.load(Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        assert_eq!(fired.load(Ordering::SeqCst), count);
    }

    #[tokio::test]
    async fn scheduler_rejects_invalid_cron() {
        let scheduler = Scheduler::new().await.unwrap();
        assert!(scheduler.add("nope", || async {}).await.is_err());
    }
}
