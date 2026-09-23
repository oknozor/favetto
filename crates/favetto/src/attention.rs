//! Watches a live agent session and mirrors "blocked on the user" onto its task.
//!
//! While a run is in flight, the executor (for catalog tasks) or the server (for
//! one-shot tasks) drives [`watch`] instead of [`AgentManager::wait`]. It polls
//! the session's terminal emulator, flips the task between `Running` and
//! `AwaitingInput`, and emits [`EventKind::TaskAwaitingInput`] so notification
//! hooks fire.
//!
//! [`AgentManager::wait`]: crate::agents::AgentManager::wait

use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

use favetto_core::model::{AwaitingInputReason, EventKind, TaskStatus};

use crate::agents::Agent;
use crate::db;
use crate::event_bus::ServerPush;
use crate::state::State;

/// How often the watcher polls the session's screen.
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Consecutive matching polls required before marking (≥2 = 1 s at
/// [`POLL_INTERVAL`]).
pub const MARK_STREAK: u32 = 2;
/// Consecutive non-matching polls required before clearing (hysteresis).
pub const CLEAR_STREAK: u32 = 3;

/// A confirmed debounced edge for one session.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Edge {
    /// `None -> Some(reason)`: mark the task as awaiting input.
    Mark(AwaitingInputReason),
    /// `Some(_) -> None`: the agent answered and resumed.
    Clear,
}

/// Temporal hysteresis over raw per-poll detection. A single transient frame
/// neither raises nor clears the signal.
#[derive(Default)]
struct Debouncer {
    confirmed: Option<AwaitingInputReason>,
    match_streak: u32,
    clear_streak: u32,
}

impl Debouncer {
    fn confirmed(&self) -> Option<&AwaitingInputReason> {
        self.confirmed.as_ref()
    }

    /// Feed one poll's raw detection; return a confirmed edge, if any. A reason
    /// change while already confirmed refreshes the stored reason but never
    /// re-emits `Mark`.
    fn observe(&mut self, next: Option<AwaitingInputReason>) -> Option<Edge> {
        match next {
            Some(reason) => {
                self.clear_streak = 0;
                if self.confirmed.is_some() {
                    self.match_streak = 0;
                    self.confirmed = Some(reason);
                    return None;
                }
                self.match_streak += 1;
                if self.match_streak >= MARK_STREAK {
                    self.match_streak = 0;
                    self.confirmed = Some(reason.clone());
                    return Some(Edge::Mark(reason));
                }
                None
            }
            None => {
                self.match_streak = 0;
                self.confirmed.as_ref()?;
                self.clear_streak += 1;
                if self.clear_streak >= CLEAR_STREAK {
                    self.clear_streak = 0;
                    self.confirmed = None;
                    return Some(Edge::Clear);
                }
                None
            }
        }
    }
}

/// Poll `session` until it exits, mirroring awaiting-input transitions onto
/// `task_id` (when given) and the manager's per-session slot. Returns the exit
/// code, like [`AgentManager::wait`].
///
/// [`AgentManager::wait`]: crate::agents::AgentManager::wait
pub async fn watch(
    state: &Arc<State>,
    session: &str,
    agent: Arc<dyn Agent>,
    task_id: Option<Uuid>,
    quiet: Duration,
) -> Option<i32> {
    let mut debouncer = Debouncer::default();
    loop {
        if !state.agents.is_running(session) {
            break;
        }
        let next = state
            .agents
            .detect_awaiting_input(session, agent.as_ref(), quiet);
        if let Some(edge) = debouncer.observe(next) {
            if let Some(task_id) = task_id {
                match edge {
                    Edge::Mark(reason) => mark_awaiting(state, task_id, session, &reason).await,
                    Edge::Clear => {
                        resume_task(state, task_id).await;
                    }
                }
            }
        }
        state
            .agents
            .set_awaiting_input(session, debouncer.confirmed().cloned());
        if let Some(task_id) = task_id {
            // Self-heal: re-assert the confirmed state every poll so an external
            // overwrite (or resurrection) is repaired within POLL_INTERVAL.
            reconcile_task(state, task_id, debouncer.confirmed().is_some()).await;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    // The process is gone: drop the slot and restore a non-terminal baseline so
    // the caller's terminal-state handling (which overwrites the row) is clean.
    state.agents.set_awaiting_input(session, None);
    if let Some(task_id) = task_id {
        resume_task(state, task_id).await;
    }
    state.agents.exit_code(session)
}

/// Conditional status write + push. Returns true when a row changed.
async fn reconcile_task(state: &Arc<State>, task_id: Uuid, awaiting: bool) -> bool {
    let (to, from) = if awaiting {
        (TaskStatus::AwaitingInput, TaskStatus::Running)
    } else {
        (TaskStatus::Running, TaskStatus::AwaitingInput)
    };
    let Ok(true) = db::set_task_status_if(&state.db, task_id, to, from).await else {
        return false;
    };
    if let Ok(Some(task)) = db::get_task(&state.db, task_id).await {
        state
            .bus
            .publish(ServerPush::TaskUpdated(Box::new(task.summary())));
    }
    true
}

/// Persist a task as `AwaitingInput` and announce the event.
async fn mark_awaiting(
    state: &Arc<State>,
    task_id: Uuid,
    session: &str,
    reason: &AwaitingInputReason,
) {
    if !reconcile_task(state, task_id, true).await {
        return; // was not Running: another writer owns the row
    }
    if let Ok(Some(task)) = db::get_task(&state.db, task_id).await {
        state
            .emit_event(
                EventKind::TaskAwaitingInput,
                serde_json::json!({
                    "task_id": task.id.to_string(),
                    "name": task.name,
                    "session_id": session,
                    "reason": reason,
                }),
            )
            .await;
    }
}

/// Restore an awaiting-input task to `Running` once the agent resumes.
async fn resume_task(state: &Arc<State>, task_id: Uuid) -> bool {
    reconcile_task(state, task_id, false).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{AgentContext, AgentManager, AgentRegistry, Invocation};
    use crate::config::{AgentConfig, FavettoConfig};
    use crate::event_bus::EventBus;
    use crate::webhooks::WebhookSecrets;
    use favetto_core::auth::Token;
    use parking_lot::RwLock;

    fn reason(kind: favetto_core::model::AwaitingInputKind) -> AwaitingInputReason {
        AwaitingInputReason {
            kind,
            message: "prompt".to_string(),
        }
    }

    #[test]
    fn one_match_then_miss_does_not_mark() {
        use favetto_core::model::AwaitingInputKind;
        let mut debouncer = Debouncer::default();
        assert_eq!(
            debouncer.observe(Some(reason(AwaitingInputKind::Permission))),
            None
        );
        assert_eq!(debouncer.observe(None), None);
        assert!(debouncer.confirmed().is_none());
    }

    #[test]
    fn mark_requires_two_consecutive_matches() {
        use favetto_core::model::AwaitingInputKind;
        let mut debouncer = Debouncer::default();
        assert_eq!(debouncer.observe(None), None);
        assert_eq!(
            debouncer.observe(Some(reason(AwaitingInputKind::Permission))),
            None
        );
        let edge = debouncer.observe(Some(reason(AwaitingInputKind::Permission)));
        assert_eq!(
            edge,
            Some(Edge::Mark(reason(AwaitingInputKind::Permission)))
        );
        assert_eq!(
            debouncer.confirmed().map(|r| r.kind),
            Some(AwaitingInputKind::Permission)
        );
    }

    #[test]
    fn clear_requires_three_consecutive_misses() {
        use favetto_core::model::AwaitingInputKind;
        let mut debouncer = Debouncer::default();
        debouncer.observe(Some(reason(AwaitingInputKind::Permission)));
        assert!(matches!(
            debouncer.observe(Some(reason(AwaitingInputKind::Permission))),
            Some(Edge::Mark(_))
        ));
        assert_eq!(debouncer.observe(None), None);
        assert_eq!(debouncer.observe(None), None);
        assert_eq!(debouncer.observe(None), Some(Edge::Clear));
        assert!(debouncer.confirmed().is_none());
    }

    #[test]
    fn reason_change_while_confirmed_does_not_remark() {
        use favetto_core::model::AwaitingInputKind;
        let mut debouncer = Debouncer::default();
        debouncer.observe(Some(reason(AwaitingInputKind::Permission)));
        debouncer.observe(Some(reason(AwaitingInputKind::Permission)));
        assert_eq!(
            debouncer.observe(Some(reason(AwaitingInputKind::Choice))),
            None
        );
        assert_eq!(
            debouncer.confirmed().map(|r| r.kind),
            Some(AwaitingInputKind::Choice)
        );
    }

    /// A `State` whose `sh` agent runs a headless session bound to a task.
    async fn state_with_sh(script: &str) -> Arc<State> {
        let dir = std::env::temp_dir().join(format!("favetto-attention-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::db::open(&dir.join("test.db")).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();

        let mut cfg = FavettoConfig::default();
        cfg.agent.default = Some("sh".to_string());
        cfg.agents.insert(
            "sh".to_string(),
            AgentConfig {
                command: "sh".to_string(),
                headless_args: Some(vec!["-c".to_string(), script.to_string()]),
                ..Default::default()
            },
        );
        let registry = AgentRegistry::from_config_with(&cfg, &|_| true).unwrap();
        Arc::new(State::new(
            pool,
            EventBus::new(64),
            Token::generate(),
            WebhookSecrets {
                github: None,
                linear: None,
            },
            AgentManager::new(),
            registry,
            Arc::new(RwLock::new(cfg)),
            dir.clone(),
            dir.clone(),
            Arc::new(RwLock::new(Vec::new())),
            tokio_cron_scheduler::JobScheduler::new().await.unwrap(),
            Arc::new(RwLock::new(Vec::new())),
        ))
    }

    #[tokio::test]
    async fn watch_flips_running_to_awaiting_and_back() {
        // Keep the prompt in the args via `{prompt}` so it is not written to the
        // PTY's stdin. A stdin prompt is echoed onto the printed line
        // ("Enter passphrase: go"), which drops the trailing `:` the generic
        // detector keys on, and the echo/print ordering is racy. The agent then
        // blocks until the test creates the marker file instead of sleeping for a
        // fixed second: `AwaitingInput` is transient, so a fixed lifetime also
        // races the watcher's 500ms poll interval.
        let release =
            std::env::temp_dir().join(format!("favetto-attention-release-{}", Uuid::new_v4()));
        let script = format!(
            "# {{prompt}}\nprintf 'Enter passphrase: '; while [ ! -e '{}' ]; do sleep 0.05; done; exit 0",
            release.display()
        );
        let state = state_with_sh(&script).await;
        let agent = state.registry.get("sh").unwrap();

        let task = favetto_core::model::Task {
            id: Uuid::new_v4(),
            name: "oneshot".to_string(),
            status: TaskStatus::Running,
            input: serde_json::json!({}),
            output: None,
            dedupe_key: None,
            created_at: chrono::Utc::now(),
            started_at: Some(chrono::Utc::now()),
            finished_at: None,
            error: None,
            session_id: None,
            session_title: None,
            parent_id: None,
            root_id: None,
        };
        db::insert_task(&state.db, &task).await.unwrap();

        let info = state
            .agents
            .start(
                "sh",
                agent.clone(),
                Some(task.id.to_string()),
                Invocation::Headless {
                    prompt: "go",
                    provider: None,
                    model: None,
                },
                AgentContext {
                    rows: 40,
                    cols: 120,
                    ..Default::default()
                },
            )
            .unwrap();

        let handle = tokio::spawn({
            let state = state.clone();
            let session = info.id.clone();
            async move {
                watch(
                    &state,
                    &session,
                    agent,
                    Some(task.id),
                    Duration::from_millis(50),
                )
                .await
            }
        });

        // The task flips to AwaitingInput while the agent blocks on the prompt.
        let mut saw_awaiting = false;
        for _ in 0..100 {
            let current = db::get_task(&state.db, task.id).await.unwrap().unwrap();
            if current.status == TaskStatus::AwaitingInput {
                saw_awaiting = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(saw_awaiting, "task never became AwaitingInput");

        // Release the blocked agent so `watch` observes the exit and restores the
        // baseline.
        std::fs::write(&release, b"go").unwrap();

        let code = handle.await.unwrap();
        assert_eq!(code, Some(0));

        // The watcher restored the non-terminal baseline on exit.
        let after = db::get_task(&state.db, task.id).await.unwrap().unwrap();
        assert_eq!(after.status, TaskStatus::Running);

        // A `task_awaiting_input` event was persisted for hooks.
        let events = db::tail_events(&state.db, 50).await.unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.kind == EventKind::TaskAwaitingInput),
            "no TaskAwaitingInput event: {events:?}"
        );

        let _ = std::fs::remove_file(&release);
    }

    #[tokio::test]
    async fn reconcile_task_self_heals_awaiting_input() {
        let state = state_with_sh("exit 0").await;
        let task = favetto_core::model::Task {
            id: Uuid::new_v4(),
            name: "oneshot".to_string(),
            status: TaskStatus::Running,
            input: serde_json::json!({}),
            output: None,
            dedupe_key: None,
            created_at: chrono::Utc::now(),
            started_at: Some(chrono::Utc::now()),
            finished_at: None,
            error: None,
            session_id: None,
            session_title: None,
            parent_id: None,
            root_id: None,
        };
        db::insert_task(&state.db, &task).await.unwrap();

        // An external writer (e.g. a stale full-row backfill) marks the task
        // awaiting input.
        assert!(db::set_task_status_if(
            &state.db,
            task.id,
            TaskStatus::AwaitingInput,
            TaskStatus::Running,
        )
        .await
        .unwrap());

        // The watcher reconciles against the live session state and repairs it.
        let mut rx = state.bus.subscribe();
        assert!(reconcile_task(&state, task.id, false).await);
        let stored = db::get_task(&state.db, task.id).await.unwrap().unwrap();
        assert_eq!(stored.status, TaskStatus::Running);
        assert!(matches!(rx.try_recv(), Ok(ServerPush::TaskUpdated(_))));

        // The converse: marking is also a targeted conditional write.
        let mut rx = state.bus.subscribe();
        assert!(reconcile_task(&state, task.id, true).await);
        let stored = db::get_task(&state.db, task.id).await.unwrap().unwrap();
        assert_eq!(stored.status, TaskStatus::AwaitingInput);
        assert!(matches!(rx.try_recv(), Ok(ServerPush::TaskUpdated(_))));

        // A no-op reconcile changes nothing and publishes nothing.
        let mut rx = state.bus.subscribe();
        assert!(!reconcile_task(&state, task.id, true).await);
        assert!(rx.try_recv().is_err());
    }
}
