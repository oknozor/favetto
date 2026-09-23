//! Notification hooks: map event + filter → notify action.
//!
//! Hooks are evaluated against every event published on the bus. The only action
//! is `notify`, which sends the event through a notification channel. Hooks are
//! **in-memory**: the TUI adds them live via `hooks.upsert`, and they are lost on
//! restart. Task triggers from GitHub now live in `[webhook.github]` (config.toml).
//!
//! ```text
//! hooks.upsert { event = "TicketCreated", channel = "webhook", config = { url = "…" } }
//! ```

use std::sync::Arc;

use parking_lot::RwLock;
use serde_json::Value;

use favetto_core::model::{Event, EventKind};

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
    Notify { channel: String, config: Value },
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

/// Subscribes to the bus and runs matching hooks against every event. The hook list
/// is shared behind a lock so new hooks (e.g. notification hooks added from the TUI)
/// take effect immediately.
pub struct HookEngine {
    hooks: Arc<RwLock<Vec<Hook>>>,
    state: Arc<State>,
}

impl HookEngine {
    pub fn new(hooks: Arc<RwLock<Vec<Hook>>>, state: Arc<State>) -> Self {
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
        let hooks = self.hooks.read().clone();
        for hook in &hooks {
            if !hook.enabled || hook.event != ev.kind || !hook.matches(&ev.payload) {
                continue;
            }
            self.run_action(hook, &ev).await;
        }
    }

    async fn run_action(&self, hook: &Hook, ev: &Event) {
        match &hook.action {
            HookAction::Notify { channel, config } => {
                crate::notify::send(
                    &self.state,
                    channel,
                    config,
                    &format!("favetto event: {}", ev.kind.as_str()),
                    &serde_json::to_string(&ev.payload).unwrap_or_default(),
                )
                .await;
            }
        }
    }
}

/// Build a notification hook that reacts to `event` and sends via `channel`.
pub fn notify_hook(event: EventKind, channel: String, config: Value) -> Hook {
    Hook {
        event,
        filter: None,
        action: HookAction::Notify { channel, config },
        enabled: true,
    }
}
