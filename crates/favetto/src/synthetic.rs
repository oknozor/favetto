//! Synthetic event driver — keeps the TUI alive until real integrations land (M3+).
//!
//! A background task creates fake tasks and advances them through their lifecycle,
//! persisting everything and broadcasting pushes so the Tasks/Events tabs have
//! something to show. Deterministic-ish (tiny xorshift PRNG) to avoid a `rand` dep.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use uuid::Uuid;

use favetto_core::model::{Event, EventKind, Task, TaskStatus};

use crate::event_bus::ServerPush;
use crate::state::State;
use crate::{db, event_bus};

const SKILLS: &[&str] = &[
    "implement_linear_ticket",
    "triage_github_issue",
    "summarize_email_thread",
];

/// Tiny deterministic PRNG so M1 has no external RNG dependency.
struct Lcg(u64);

impl Lcg {
    fn new() -> Self {
        Lcg(Utc::now().timestamp_millis() as u64 | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }

    fn range(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn chance(&mut self, pct: u64) -> bool {
        self.range(100) < pct
    }
}

/// Run the driver forever (spawned by the daemon).
pub async fn run(state: Arc<State>) {
    let mut interval = tokio::time::interval(Duration::from_secs(2));
    let mut rng = Lcg::new();
    let mut in_flight: Vec<Task> = Vec::new();

    loop {
        interval.tick().await;

        // Create a new synthetic task, bounded to keep the TUI readable.
        if in_flight.len() < 5 && rng.chance(60) {
            let skill = SKILLS[rng.range(SKILLS.len() as u64) as usize].to_string();
            let task = Task {
                id: Uuid::new_v4(),
                skill,
                status: TaskStatus::Pending,
                input: serde_json::json!({ "synthetic": true, "seed": rng.range(10_000) }),
                output: None,
                dedupe_key: None,
                created_at: Utc::now(),
                started_at: None,
                finished_at: None,
                error: None,
            };

            if db::insert_task(&state.db, &task).await.is_ok() {
                state.bus.publish(ServerPush::TaskUpdated(task.clone()));
                emit_event(&state, EventKind::TaskCreated, serde_json::json!({ "task_id": task.id })).await;
                in_flight.push(task);
            }
        }

        // Advance each in-flight task; terminal tasks drop out of the list.
        let mut next: Vec<Task> = Vec::new();
        for mut task in in_flight.drain(..) {
            match advance(&mut rng, &mut task) {
                Advance::None => next.push(task),
                Advance::Update(kind) => {
                    let _ = db::upsert_task(&state.db, &task).await;
                    state.bus.publish(ServerPush::TaskUpdated(task.clone()));
                    emit_event(&state, kind, serde_json::json!({ "task_id": task.id })).await;
                    if task.status == TaskStatus::Running {
                        next.push(task);
                    }
                }
            }
        }
        in_flight = next;

        // Occasional cron tick to exercise the event pipeline.
        if rng.chance(15) {
            emit_event(
                &state,
                EventKind::CronTick,
                serde_json::json!({ "schedule": "synthetic" }),
            )
            .await;
        }
    }
}

enum Advance {
    None,
    Update(EventKind),
}

fn advance(rng: &mut Lcg, task: &mut Task) -> Advance {
    if !rng.chance(40) {
        return Advance::None;
    }
    match task.status {
        TaskStatus::Pending => {
            task.status = TaskStatus::Running;
            task.started_at = Some(Utc::now());
            Advance::Update(EventKind::TaskUpdated)
        }
        TaskStatus::Running => {
            task.finished_at = Some(Utc::now());
            if rng.chance(80) {
                task.status = TaskStatus::Succeeded;
                task.output = Some(serde_json::json!({ "result": "ok" }));
                Advance::Update(EventKind::TaskCompleted)
            } else {
                task.status = TaskStatus::Failed;
                task.error = Some("synthetic failure".to_string());
                Advance::Update(EventKind::TaskFailed)
            }
        }
        _ => Advance::None,
    }
}

/// Persist an event and broadcast it, wiring the returned monotonic id back in.
async fn emit_event(state: &State, kind: EventKind, payload: serde_json::Value) {
    let event = Event {
        id: 0,
        kind,
        payload,
        created_at: Utc::now(),
    };
    match db::insert_event(&state.db, &event).await {
        Ok(id) => {
            let event = Event { id, ..event };
            state.bus.publish(event_bus::ServerPush::Event(event));
        }
        Err(e) => tracing::warn!(error = %e, "failed to persist synthetic event"),
    }
}
