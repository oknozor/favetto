//! Reference external supervisor: observe, decide, act — entirely outside the daemon.
//!
//! This is the reusable engine behind the
//! [`reference_supervisor`](../examples/reference_supervisor.rs) example. It is
//! a plain remote-API client, exactly like the TUI: it links only `favetto-core`
//! wire types and the [`Client`], never daemon code. The daemon stays
//! deterministic; every bit of judgment lives here.
//!
//! The loop follows the [supervisor contract](../../../docs/reference/supervisor-contract.md):
//!
//! 1. observe the root with `workflow.inspect`;
//! 2. emit exactly one [`Decision`];
//! 3. apply it (`wait` mutates nothing; `spawn` calls `workflow.spawn`).
//!
//! The example here is a **two-step** pipeline: a first catalog task runs to
//! completion, then the supervisor spawns a second task into the same root and
//! waits for it. The daemon never sees the edge — the controller owns it.

use std::time::Duration;

use anyhow::Context;
use serde_json::{json, Value};
use uuid::Uuid;

use favetto_core::model::TaskStatus;
use favetto_core::rpc::{method, Response};
use favetto_core::workflow::{WorkflowInspect, WorkflowState};

use crate::client::Client;

/// The closed decision vocabulary from the supervisor contract.
///
/// `Inspect`, `Spawn`, `Cancel` and `Retry` map onto daemon RPCs; `Wait`,
/// `RequestInput`, `Complete` and `Escalate` are supervisor-side states that send
/// nothing. The reference implementation only emits `Wait`, `Spawn`, `Complete`
/// and `Escalate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Refresh the observation (`workflow.inspect`).
    Inspect,
    /// Add work to the root (`workflow.spawn`).
    Spawn,
    /// Abandon the root (`workflow.cancel`).
    Cancel,
    /// Re-run a terminal task (`workflow.retry`).
    Retry,
    /// Do nothing; keep observing.
    Wait,
    /// Surface a human decision the supervisor cannot make.
    RequestInput,
    /// The root finished as expected; stop supervising it.
    Complete,
    /// Stop autonomous control and hand off to a human.
    Escalate,
}

/// One decision: exactly one action, a human-readable reason, and the arguments
/// the mapped RPC receives. Mirrors the decision object in the contract.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub action: Action,
    /// Audit trail; the daemon never reads it.
    pub reason: String,
    /// Action-specific arguments. `null` for the local actions.
    pub params: Value,
}

/// The two-step policy the reference supervisor drives.
///
/// The first task is started as the root; once it succeeds, the second is
/// spawned into the same root and awaited. Dedupe makes a re-issued spawn a
/// no-op, so the decision can be retried safely.
#[derive(Debug, Clone)]
pub struct Policy {
    /// Catalog task to start as the root (the first step).
    pub first_task: String,
    /// Catalog task spawned once `first_task` succeeds (the second step).
    pub second_task: String,
    /// Input passed to the spawned second step. Skipped when JSON `null`.
    pub input: Value,
    /// Dedupe key for the spawned second step.
    pub dedupe_key: String,
    /// Pause between inspections when the decision is [`Action::Wait`].
    pub wait: Duration,
    /// Safety bound on the number of decision cycles.
    pub max_cycles: usize,
}

impl Policy {
    /// A policy for the `examples/supervisor` catalog tasks.
    pub fn example() -> Self {
        Self {
            first_task: "examples/supervisor/plan".to_string(),
            second_task: "examples/supervisor/implement".to_string(),
            input: Value::Null,
            dedupe_key: "examples/supervisor:implement".to_string(),
            wait: Duration::from_millis(250),
            max_cycles: 240,
        }
    }
}

/// The decisions taken during one supervised run, plus the terminal one.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    /// Every decision in order, including `wait`.
    pub decisions: Vec<Decision>,
    /// The `complete` or `escalate` that ended the run.
    pub terminal: Decision,
}

/// Decide the next action from one observation.
///
/// `spawned` is `true` once this supervisor has already submitted the second
/// step, so an inspection that has not caught up yet does not re-issue it.
pub fn decide(view: &WorkflowInspect, policy: &Policy, spawned: bool) -> Decision {
    // A failure or cancellation anywhere in the root is terminal for the
    // reference policy: hand off to a human with the failing task named.
    if let Some(failed) = view.tasks.iter().find(|t| t.status == TaskStatus::Failed) {
        let summary = failed.summary.clone().unwrap_or_default();
        let detail = if summary.is_empty() {
            String::new()
        } else {
            format!(": {summary}")
        };
        return Decision {
            action: Action::Escalate,
            reason: format!("task '{}' failed{detail}", failed.name),
            params: json!({ "root_id": view.root_id, "summary": summary }),
        };
    }
    if let Some(cancelled) = view
        .tasks
        .iter()
        .find(|t| t.status == TaskStatus::Cancelled)
    {
        return Decision {
            action: Action::Escalate,
            reason: format!("task '{}' was cancelled", cancelled.name),
            params: json!({ "root_id": view.root_id, "summary": "root cancelled" }),
        };
    }

    // Decide from the two named tasks, not from the root's aggregate state: the
    // root reads `succeeded` as soon as the first step is terminal, even though
    // the policy still has a second step to spawn. `complete` is therefore only
    // reachable once the second step itself has succeeded.
    if let Some(second) = view.tasks.iter().find(|t| t.name == policy.second_task) {
        return match second.status {
            TaskStatus::Succeeded => Decision {
                action: Action::Complete,
                reason: format!(
                    "root '{}' succeeded: '{}' and '{}' are done",
                    view.root_task, policy.first_task, policy.second_task
                ),
                params: json!({ "root_id": view.root_id }),
            },
            // Failures and cancellations were handled above.
            status => Decision {
                action: Action::Wait,
                reason: format!(
                    "'{}' is {}; waiting for it to finish",
                    second.name,
                    status.as_str()
                ),
                params: Value::Null,
            },
        };
    }

    let first = view
        .tasks
        .iter()
        .find(|t| t.name == policy.first_task)
        .map(|t| t.status);
    match (first, spawned) {
        (Some(TaskStatus::Succeeded), false) => Decision {
            action: Action::Spawn,
            reason: format!(
                "'{}' succeeded; spawning '{}' into the same root",
                policy.first_task, policy.second_task
            ),
            params: spawn_params(policy, view.root_id),
        },
        (Some(TaskStatus::Succeeded), true) => Decision {
            action: Action::Wait,
            reason: format!(
                "'{}' succeeded; waiting for '{}' to appear",
                policy.first_task, policy.second_task
            ),
            params: Value::Null,
        },
        (Some(status), _) => Decision {
            action: Action::Wait,
            reason: format!(
                "waiting for '{}' ({}) [root {}]",
                policy.first_task,
                status.as_str(),
                state_word(view.state)
            ),
            params: Value::Null,
        },
        (None, _) => Decision {
            action: Action::Wait,
            reason: format!("waiting for '{}' to appear", policy.first_task),
            params: Value::Null,
        },
    }
}

/// `workflow.spawn` params for the second step.
fn spawn_params(policy: &Policy, root_id: Uuid) -> Value {
    let mut params = json!({
        "root_id": root_id,
        "name": policy.second_task,
        "dedupe_key": policy.dedupe_key,
    });
    if !policy.input.is_null() {
        params["input"] = policy.input.clone();
    }
    params
}

/// Start `policy.first_task` as a fresh root and return its id.
pub async fn start_root(client: &Client, policy: &Policy) -> anyhow::Result<Uuid> {
    let resp = client
        .request(method::TASKS_START, json!({ "name": policy.first_task }))
        .await?;
    let result = ok_result(&resp, method::TASKS_START)?;
    let id = result
        .get("id")
        .and_then(Value::as_str)
        .with_context(|| format!("{} result has no task id", method::TASKS_START))?;
    Ok(Uuid::parse_str(id)?)
}

/// Fetch the runtime view of `root_id` via `workflow.inspect`.
pub async fn inspect(client: &Client, root_id: Uuid) -> anyhow::Result<WorkflowInspect> {
    let resp = client
        .request(method::WORKFLOW_INSPECT, json!({ "root_id": root_id }))
        .await?;
    let result = ok_result(&resp, method::WORKFLOW_INSPECT)?;
    serde_json::from_value(result).context("decode workflow.inspect result")
}

/// Drive a root to a terminal decision, applying each [`Decision`] in turn.
pub async fn run(client: &Client, policy: &Policy, root_id: Uuid) -> anyhow::Result<RunOutcome> {
    let mut spawned = false;
    let mut decisions = Vec::new();

    for _ in 0..policy.max_cycles {
        let view = inspect(client, root_id).await?;
        let decision = decide(&view, policy, spawned);
        match decision.action {
            Action::Wait => {
                decisions.push(decision);
                tokio::time::sleep(policy.wait).await;
            }
            Action::Inspect => {
                decisions.push(decision);
            }
            Action::Spawn => {
                spawn_second(client, policy, root_id).await?;
                spawned = true;
                decisions.push(decision);
            }
            Action::Complete | Action::Escalate => {
                decisions.push(decision.clone());
                return Ok(RunOutcome {
                    decisions,
                    terminal: decision,
                });
            }
            other => anyhow::bail!("reference supervisor does not implement {other:?}"),
        }
    }

    anyhow::bail!(
        "supervisor did not reach a terminal decision in {} cycles",
        policy.max_cycles
    )
}

/// Submit the second step into the root, deduped on `policy.dedupe_key`.
async fn spawn_second(client: &Client, policy: &Policy, root_id: Uuid) -> anyhow::Result<()> {
    let resp = client
        .request(method::WORKFLOW_SPAWN, spawn_params(policy, root_id))
        .await?;
    ok_result(&resp, method::WORKFLOW_SPAWN)?;
    Ok(())
}

/// Unwrap a successful RPC response, surfacing the wire error otherwise.
fn ok_result(resp: &Response, method: &str) -> anyhow::Result<Value> {
    if let Some(error) = &resp.error {
        anyhow::bail!("{method} failed ({}): {}", error.code, error.message);
    }
    resp.result
        .clone()
        .ok_or_else(|| anyhow::anyhow!("{method} returned no result"))
}

/// Lower-case wire name for a root state, for reasons.
fn state_word(state: WorkflowState) -> &'static str {
    match state {
        WorkflowState::Running => "running",
        WorkflowState::Succeeded => "succeeded",
        WorkflowState::Failed => "failed",
        WorkflowState::Cancelled => "cancelled",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use favetto_core::workflow::WorkflowTaskView;

    fn view(state: WorkflowState, tasks: Vec<WorkflowTaskView>) -> WorkflowInspect {
        WorkflowInspect {
            root_id: Uuid::nil(),
            root_task: "plan".to_string(),
            state,
            ready: Vec::new(),
            running: Vec::new(),
            failed: Vec::new(),
            blocked: Vec::new(),
            tasks,
        }
    }

    fn task(name: &str, status: TaskStatus, summary: Option<&str>) -> WorkflowTaskView {
        WorkflowTaskView {
            id: Uuid::new_v4(),
            name: name.to_string(),
            status,
            attempt: 1,
            summary: summary.map(str::to_string),
        }
    }

    #[test]
    fn waits_while_the_first_step_runs() {
        let policy = Policy::example();
        let v = view(
            WorkflowState::Running,
            vec![task(&policy.first_task, TaskStatus::Running, None)],
        );
        let decision = decide(&v, &policy, false);
        assert_eq!(decision.action, Action::Wait);
        assert!(decision.reason.contains("examples/supervisor/plan"));
    }

    #[test]
    fn spawns_the_second_step_once_the_first_succeeds() {
        let policy = Policy::example();
        // The root already reads `succeeded` once the first step is terminal;
        // the policy must still spawn the second step before completing.
        let v = view(
            WorkflowState::Succeeded,
            vec![task(&policy.first_task, TaskStatus::Succeeded, None)],
        );
        let decision = decide(&v, &policy, false);
        assert_eq!(decision.action, Action::Spawn);
        assert_eq!(decision.params["name"], policy.second_task);
        assert_eq!(decision.params["dedupe_key"], policy.dedupe_key);

        // Re-deciding before the spawn is observed must not spawn twice.
        assert_eq!(decide(&v, &policy, true).action, Action::Wait);
    }

    #[test]
    fn waits_for_the_spawned_second_step() {
        let policy = Policy::example();
        let v = view(
            WorkflowState::Running,
            vec![
                task(&policy.first_task, TaskStatus::Succeeded, None),
                task(&policy.second_task, TaskStatus::Running, None),
            ],
        );
        assert_eq!(decide(&v, &policy, true).action, Action::Wait);
    }

    #[test]
    fn completes_when_the_root_succeeds() {
        let policy = Policy::example();
        let v = view(
            WorkflowState::Succeeded,
            vec![
                task(&policy.first_task, TaskStatus::Succeeded, None),
                task(&policy.second_task, TaskStatus::Succeeded, None),
            ],
        );
        let decision = decide(&v, &policy, true);
        assert_eq!(decision.action, Action::Complete);
    }

    #[test]
    fn escalates_on_failure_with_the_summary() {
        let policy = Policy::example();
        let v = view(
            WorkflowState::Failed,
            vec![task(
                &policy.second_task,
                TaskStatus::Failed,
                Some("agent exited 1"),
            )],
        );
        let decision = decide(&v, &policy, true);
        assert_eq!(decision.action, Action::Escalate);
        assert!(decision.reason.contains("agent exited 1"), "{decision:?}");
    }
}
