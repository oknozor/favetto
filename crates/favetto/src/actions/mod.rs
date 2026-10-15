//! Built-in, deterministic in-daemon actions.
//!
//! A catalog task with `kind = "action"` runs one of these instead of launching
//! an external agent: [`Action::execute`] performs the step (an HTTP request
//! today) and returns a structured result that is folded into the same
//! `task.output` envelope an agent run produces. An optional `[action.when]`
//! predicate over that result gates the task's `spawn` handoff, so a poller can
//! decide, reproducibly, whether to start the next task.
//!
//! This mirrors the [`crate::agents`] registry. Only curated built-ins are
//! exposed: there is deliberately no shell or arbitrary-command action.

mod http;
mod registry;
mod when;

use std::collections::BTreeMap;
use std::path::PathBuf;

use futures_util::future::BoxFuture;
use uuid::Uuid;

pub use registry::ActionRegistry;
pub use when::evaluate_when;

use favetto_core::model::FailureKind;

/// Per-execution context handed to an [`Action`].
#[derive(Debug, Clone)]
pub struct ActionContext {
    /// The rendered `[action.params]` table, as JSON.
    pub params: serde_json::Value,
    /// The directory the task is running in.
    pub cwd: PathBuf,
    /// The running task's id, for diagnostics.
    pub task_id: Uuid,
    /// Byte cap for the action's captured output, from
    /// `[executor].max_output_bytes`.
    pub max_output_bytes: usize,
    /// Environment snapshot used to expand `$VAR` / `${VAR}` in parameter values
    /// (currently header values). Captured once so a run cannot observe a
    /// half-updated environment and so tests can inject a deterministic one.
    pub env: BTreeMap<String, String>,
}

/// The structured result of an action, folded into `task.output.envelope`.
#[derive(Debug, Clone)]
pub struct ActionOutcome {
    /// One-line human-readable summary (also stored in the envelope).
    pub summary: String,
    /// Structured values exposed to dependents as `envelope.outputs` and to a
    /// `[action.when]` predicate.
    pub outputs: serde_json::Value,
}

/// A failed action, tagged with a [`FailureKind`] so the executor can classify
/// the task failure exactly like an agent failure.
#[derive(Debug, Clone)]
pub struct ActionError {
    pub kind: FailureKind,
    pub message: String,
}

impl ActionError {
    /// The action's input was invalid (unknown id, missing/invalid params).
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::InvalidInput,
            message: message.into(),
        }
    }

    /// The action could not complete for an environmental reason (network
    /// failure, timeout, unset secret, …).
    pub fn infrastructure(message: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::Infrastructure,
            message: message.into(),
        }
    }
}

/// A built-in in-daemon step. Mirrors [`crate::agents::Agent`]: `id` is the
/// registry key (`http.request`) and `execute` performs the step.
pub trait Action: Send + Sync {
    /// The registry id, e.g. `http.request`.
    fn id(&self) -> &str;

    /// Run the action. The returned future is boxed so the trait stays
    /// dyn-compatible across the registry.
    fn execute<'a>(
        &'a self,
        ctx: &'a ActionContext,
    ) -> BoxFuture<'a, Result<ActionOutcome, ActionError>>;
}
