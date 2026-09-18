//! Hook engine: map event + filter → action, defined in `hooks.toml`.
//!
//! Hooks are evaluated against every event published on the bus. Supported actions:
//! - `run_skill` — enqueue a task for the named skill (executed by the scheduler in M5).
//! - `emit_event` — emit a new derived event.
//! - `notify` — send a notification (M5).
//!
//! ```toml
//! [[hooks]]
//! event = "TicketCreated"
//! filter = { type = "Issue" }
//! action = { type = "run_skill", skill = "implement_linear_ticket" }
//! ```

use std::path::Path;
use std::sync::Arc;

use chrono::Utc;
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use favetto_core::model::{Event, EventKind, Task, TaskStatus};

use crate::db;
use crate::event_bus::ServerPush;
use crate::state::State;

#[derive(Debug, Clone)]
pub struct Hook {
    pub event: EventKind,
    pub filter: Option<Value>,
    pub action: HookAction,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub enum HookAction {
    RunSkill { skill: String },
    EmitEvent { kind: EventKind },
    Notify { channel: String },
}

impl Hook {
    /// Shallow object filter: every key in `filter` must equal the same key in the
    /// event payload.
    fn matches(&self, payload: &Value) -> bool {
        match &self.filter {
            None => true,
            Some(Value::Object(filter)) => filter
                .iter()
                .all(|(k, expected)| payload.get(k) == Some(expected)),
            Some(_) => false,
        }
    }
}

#[derive(Debug, Deserialize)]
struct HookFile {
    #[serde(default)]
    hooks: Vec<HookToml>,
}

#[derive(Debug, Deserialize)]
struct HookToml {
    event: String,
    #[serde(default)]
    filter: Option<Value>,
    action: ActionToml,
    #[serde(default = "default_true")]
    enabled: bool,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ActionToml {
    RunSkill { skill: String },
    EmitEvent { kind: String },
    Notify { channel: String },
}

fn default_true() -> bool {
    true
}

/// Load hooks from `hooks.toml`. A missing file yields an empty list.
pub fn load_hooks(path: &Path) -> anyhow::Result<Vec<Hook>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!("no hooks file at {}", path.display());
            return Ok(Vec::new());
        }
        Err(e) => return Err(e.into()),
    };

    let file: HookFile = toml::from_str(&raw)?;
    file.hooks
        .into_iter()
        .map(|h| {
            let event = EventKind::from_name(&h.event)
                .ok_or_else(|| anyhow::anyhow!("unknown hook event kind: {}", h.event))?;
            let action = match h.action {
                ActionToml::RunSkill { skill } => HookAction::RunSkill { skill },
                ActionToml::EmitEvent { kind } => HookAction::EmitEvent {
                    kind: EventKind::from_name(&kind)
                        .ok_or_else(|| anyhow::anyhow!("unknown hook action event kind: {kind}"))?,
                },
                ActionToml::Notify { channel } => HookAction::Notify { channel },
            };
            Ok(Hook {
                event,
                filter: h.filter,
                action,
                enabled: h.enabled,
            })
        })
        .collect()
}

/// Subscribes to the bus and runs matching hooks against every event.
pub struct HookEngine {
    hooks: Vec<Hook>,
    state: Arc<State>,
}

impl HookEngine {
    pub fn new(hooks: Vec<Hook>, state: Arc<State>) -> Self {
        Self { hooks, state }
    }

    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut rx = self.state.bus.subscribe();
            loop {
                match rx.recv().await {
                    Ok(ServerPush::Event(ev)) => self.evaluate(ev).await,
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(n, "hook engine lagged behind event stream");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    async fn evaluate(&self, ev: Event) {
        for hook in &self.hooks {
            if !hook.enabled || hook.event != ev.kind || !hook.matches(&ev.payload) {
                continue;
            }
            self.run_action(hook, &ev).await;
        }
    }

    async fn run_action(&self, hook: &Hook, ev: &Event) {
        match &hook.action {
            HookAction::RunSkill { skill } => {
                let task = Task {
                    id: Uuid::new_v4(),
                    skill: skill.clone(),
                    status: TaskStatus::Pending,
                    input: ev.payload.clone(),
                    output: None,
                    dedupe_key: Some(format!("hook:{}:{}", skill, ev.id)),
                    created_at: Utc::now(),
                    started_at: None,
                    finished_at: None,
                    error: None,
                };
                match db::insert_task(&self.state.db, &task).await {
                    Ok(()) => {
                        tracing::info!(skill, event_id = ev.id, "hook enqueued task");
                        self.state.bus.publish(ServerPush::TaskUpdated(task));
                    }
                    Err(e) => tracing::warn!(error = %e, "hook failed to enqueue task"),
                }
            }
            HookAction::EmitEvent { kind } => {
                self.state
                    .emit_event(kind.clone(), serde_json::json!({ "source_event_id": ev.id }))
                    .await;
            }
            HookAction::Notify { channel } => {
                tracing::debug!(channel, event_id = ev.id, "notify hook is deferred to M5");
            }
        }
    }
}
