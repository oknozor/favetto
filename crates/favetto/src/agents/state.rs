//! The live agent-state adapter seam.
//!
//! Every supported agent CLI exposes a machine-readable channel (HTTP/SSE, JSONL
//! over stdio, hooks) beside the rendered screen. [`StateSource`] is the seam
//! that lets an adapter normalize that channel into [`AgentStateEvent`]s while
//! the PTY/`vt100` pipeline remains the source of truth for what the user sees
//! and types. See `docs/design/agent-state-adapters.md` §4.
//!
//! A source is optional: [`Agent::state_source`](super::Agent::state_source)
//! defaults to `None`, and a session without one keeps the debounced screen
//! heuristic in [`crate::attention`]. Only the folded [`AgentLiveState`]
//! snapshot travels on the wire; raw events stay daemon-internal.
//!
//! The traits and structs here are the seam consumed by the per-agent
//! transports (opencode server, pi rpc, claude hooks, …) added in later phases;
//! until one lands, some fields/methods have no in-tree reader yet.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use favetto_core::model::{AgentActivity, AgentStateEvent, AgentUsage, InputReply, RunSummary};

/// A transport that produces normalized state for one agent session.
pub trait StateSource: Send + Sync {
    /// Stable label for diagnostics (`"opencode-server"`, `"pi-rpc"`, …).
    fn label(&self) -> &'static str;

    /// Start observing. Called once, after the child is spawned.
    fn start(&self, ctx: StateContext) -> anyhow::Result<StateStart>;
}

/// Everything a source may need about the launch it is observing.
pub struct StateContext {
    /// favetto's own session id (the [`AgentManager`](super::AgentManager) key).
    pub favetto_session: String,
    /// The agent's external session id, when known before launch.
    pub external_session: Option<String>,
    pub headless: bool,
    pub cwd: Option<PathBuf>,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Write access to the agent's stdin (PTY master or pipe).
    pub stdin: Arc<parking_lot::Mutex<Box<dyn Write + Send>>>,
}

/// The resolved launch description handed to
/// [`Agent::state_source`](super::Agent::state_source).
///
/// It lets an implementation choose between transports (e.g. opencode: server
/// when a session exists, stdout JSONL otherwise) without knowing how the
/// command was rendered.
#[derive(Debug, Clone, Default)]
pub struct StateSourceConfig {
    /// Whether this is an unattended run (machine-readable output) rather than
    /// an interactive TUI.
    pub headless: bool,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
}

/// Stops a detached transport (server/SSE/hook) when dropped or called.
pub type StateStop = Box<dyn FnOnce() + Send + Sync>;

/// The started state channel for one session.
pub struct StateStart {
    /// Normalized events, consumed by the session's state task.
    pub events: mpsc::UnboundedReceiver<AgentStateEvent>,
    /// An incremental stdout parser; the manager feeds it every raw chunk.
    pub stdout: Option<Box<dyn StdoutParser>>,
    /// Answers prompts through this transport.
    pub responder: Arc<dyn InputResponder>,
    /// Stops a detached transport (server/SSE/hook) early; dropped on close.
    pub stop: Option<StateStop>,
}

/// Incremental parser for a line-delimited JSON stream.
pub trait StdoutParser: Send {
    /// Feed a raw output chunk; complete rows are parsed immediately.
    fn push(&mut self, chunk: &[u8]);
    /// End of stream; emit `Idle`/`Usage`/`Session` as appropriate.
    fn finish(&mut self, exit_code: Option<i32>);
    /// The structured summary so far, for the final task output.
    fn summary(&self) -> RunSummary;
}

/// Answers a structured input request through its transport.
pub trait InputResponder: Send + Sync {
    /// Reply to the request identified by `request_id`.
    fn reply<'a>(
        &'a self,
        request_id: &'a str,
        reply: InputReply,
    ) -> BoxFuture<'a, anyhow::Result<()>>;
}

/// The folded live snapshot of a session: what it is doing and what it cost.
///
/// This is the only state shape that travels on the wire (on `agents.list` and
/// the scoped `agent.state` push); raw [`AgentStateEvent`]s stay internal.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentLiveState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<AgentActivity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<AgentUsage>,
}

impl AgentLiveState {
    /// Fold one normalized event into the snapshot.
    pub fn apply(&mut self, event: &AgentStateEvent) {
        match event {
            AgentStateEvent::TurnStarted | AgentStateEvent::ReasoningDelta { .. } => {
                self.activity = Some(AgentActivity::Thinking);
            }
            AgentStateEvent::TextDelta { .. } => {
                self.activity = Some(AgentActivity::Responding);
            }
            AgentStateEvent::ToolStarted { name, .. } => {
                self.activity = Some(AgentActivity::Tool {
                    name: name.clone(),
                    description: None,
                });
            }
            // A partial tool update does not change the coarse activity.
            AgentStateEvent::ToolUpdated { .. } => {}
            AgentStateEvent::ToolFinished { .. } => {
                self.activity = Some(AgentActivity::Thinking);
            }
            AgentStateEvent::InputRequested { request } => {
                self.activity = Some(AgentActivity::Waiting {
                    request: request.clone(),
                });
            }
            AgentStateEvent::InputResolved { .. } | AgentStateEvent::Idle { .. } => {
                self.activity = Some(AgentActivity::Idle);
            }
            AgentStateEvent::Usage { usage } => {
                self.usage
                    .get_or_insert_with(AgentUsage::default)
                    .merge(usage);
            }
            // A session-level error is informational: the task lifecycle
            // (exit code / Idle outcome) decides success, not this snapshot.
            AgentStateEvent::Error { .. } => {}
            AgentStateEvent::Session { .. } | AgentStateEvent::Title { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use favetto_core::model::IdleOutcome;

    #[test]
    fn live_state_folds_activity_and_usage() {
        let mut live = AgentLiveState::default();
        assert!(live.activity.is_none());

        live.apply(&AgentStateEvent::TurnStarted);
        assert_eq!(live.activity, Some(AgentActivity::Thinking));

        live.apply(&AgentStateEvent::TextDelta {
            role: favetto_core::model::MessageRole::Assistant,
            text: "hi".to_string(),
        });
        assert_eq!(live.activity, Some(AgentActivity::Responding));

        live.apply(&AgentStateEvent::ToolStarted {
            id: "c1".to_string(),
            name: "bash".to_string(),
            input: serde_json::json!({}),
        });
        assert_eq!(
            live.activity,
            Some(AgentActivity::Tool {
                name: "bash".to_string(),
                description: None,
            })
        );

        live.apply(&AgentStateEvent::Usage {
            usage: AgentUsage {
                input_tokens: 3,
                cost_usd: Some(0.01),
                ..Default::default()
            },
        });
        live.apply(&AgentStateEvent::Usage {
            usage: AgentUsage {
                output_tokens: 2,
                ..Default::default()
            },
        });
        let usage = live.usage.as_ref().unwrap();
        assert_eq!(usage.input_tokens, 3);
        assert_eq!(usage.output_tokens, 2);
        assert_eq!(usage.cost_usd, Some(0.01));

        live.apply(&AgentStateEvent::Idle {
            outcome: IdleOutcome::Succeeded,
        });
        assert_eq!(live.activity, Some(AgentActivity::Idle));
    }

    #[test]
    fn live_state_marks_waiting_on_input_request() {
        let mut live = AgentLiveState::default();
        let request = favetto_core::model::InputRequest {
            id: "perm_1".to_string(),
            kind: favetto_core::model::AwaitingInputKind::Permission,
            message: "Allow?".to_string(),
            options: vec!["Allow".to_string()],
            allow_always: true,
        };
        live.apply(&AgentStateEvent::InputRequested {
            request: request.clone(),
        });
        assert_eq!(
            live.activity,
            Some(AgentActivity::Waiting {
                request: request.clone()
            })
        );

        live.apply(&AgentStateEvent::InputResolved {
            id: "perm_1".to_string(),
        });
        assert_eq!(live.activity, Some(AgentActivity::Idle));
    }

    #[test]
    fn live_state_round_trips_and_omits_empty_fields() {
        let json = serde_json::to_value(AgentLiveState::default()).unwrap();
        assert!(json.get("activity").is_none());
        assert!(json.get("usage").is_none());

        let live = AgentLiveState {
            activity: Some(AgentActivity::Starting),
            usage: Some(AgentUsage {
                input_tokens: 1,
                ..Default::default()
            }),
        };
        let back: AgentLiveState =
            serde_json::from_value(serde_json::to_value(&live).unwrap()).unwrap();
        assert_eq!(back, live);
    }
}
