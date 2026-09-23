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

/// The direction of an awaiting-input transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transition {
    /// `None -> Some(reason)`: mark the task as awaiting input.
    Mark,
    /// `Some(_) -> None`: the agent answered and resumed.
    Clear,
    /// No terminal status change (including a reason changing kind).
    None,
}

fn classify(
    current: &Option<AwaitingInputReason>,
    next: &Option<AwaitingInputReason>,
) -> Transition {
    match (current.is_some(), next.is_some()) {
        (false, true) => Transition::Mark,
        (true, false) => Transition::Clear,
        _ => Transition::None,
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
    let mut current: Option<AwaitingInputReason> = None;
    loop {
        if !state.agents.is_running(session) {
            break;
        }
        let reason = state
            .agents
            .detect_awaiting_input(session, agent.as_ref(), quiet);
        if reason != current {
            state.agents.set_awaiting_input(session, reason.clone());
            if let Some(task_id) = task_id {
                match classify(&current, &reason) {
                    Transition::Mark => {
                        if let Some(reason) = &reason {
                            mark_awaiting(state, task_id, session, reason).await;
                        }
                    }
                    Transition::Clear => resume_task(state, task_id).await,
                    Transition::None => {}
                }
            }
            current = reason;
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

/// Persist a task as `AwaitingInput` and announce the event.
async fn mark_awaiting(
    state: &Arc<State>,
    task_id: Uuid,
    session: &str,
    reason: &AwaitingInputReason,
) {
    let Ok(Some(mut task)) = db::get_task(&state.db, task_id).await else {
        return;
    };
    if task.status != TaskStatus::Running {
        return;
    }
    task.status = TaskStatus::AwaitingInput;
    if db::upsert_task(&state.db, &task).await.is_ok() {
        state.bus.publish(ServerPush::TaskUpdated(task.summary()));
    }
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

/// Restore an awaiting-input task to `Running` once the agent resumes.
async fn resume_task(state: &Arc<State>, task_id: Uuid) {
    let Ok(Some(mut task)) = db::get_task(&state.db, task_id).await else {
        return;
    };
    if task.status != TaskStatus::AwaitingInput {
        return;
    }
    task.status = TaskStatus::Running;
    if db::upsert_task(&state.db, &task).await.is_ok() {
        state.bus.publish(ServerPush::TaskUpdated(task.summary()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{AgentContext, AgentManager, AgentRegistry, Invocation};
    use crate::config::{AgentConfig, FavettoConfig};
    use crate::event_bus::EventBus;
    use crate::webhooks::WebhookSecrets;
    use favetto_core::auth::Token;
    use std::sync::RwLock;

    fn reason(kind: favetto_core::model::AwaitingInputKind) -> AwaitingInputReason {
        AwaitingInputReason {
            kind,
            message: "prompt".to_string(),
        }
    }

    #[test]
    fn transition_classification() {
        use favetto_core::model::AwaitingInputKind;
        assert_eq!(
            classify(&None, &Some(reason(AwaitingInputKind::Permission))),
            Transition::Mark
        );
        assert_eq!(
            classify(&Some(reason(AwaitingInputKind::Permission)), &None),
            Transition::Clear
        );
        assert_eq!(classify(&None, &None), Transition::None);
        assert_eq!(
            classify(
                &Some(reason(AwaitingInputKind::Permission)),
                &Some(reason(AwaitingInputKind::Choice))
            ),
            Transition::None
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
}
