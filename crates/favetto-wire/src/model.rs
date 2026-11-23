//! Domain model types shared across the daemon and TUI client.

use std::path::PathBuf;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::rpc::{push, Notification};

pub use crate::agent_state::{
    AgentActivity, AgentCommand, AgentCost, AgentPlanEntry, AgentPlanPriority, AgentPlanStatus,
    AgentStateEvent, AgentUsage, ContentRole, IdleOutcome, InputReply, InputRequest,
    MessageContent, RunSummary, SessionConfigChoice, SessionConfigChoices, SessionConfigGroup,
    SessionConfigKind, SessionConfigOption, SessionConfigValue, SessionMode, SessionModes,
    ToolCall, ToolCallDetail, ToolCallStatus, ToolContent, ToolKind, ToolLocation, UsageBucket,
    UsagePeriod, UsageStats, UsageTotals,
};

/// Lifecycle of a single task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Queued, not yet picked up by the agent runtime.
    Pending,
    /// Currently being executed by the agent runtime.
    Running,
    /// Running, but the agent is blocked waiting for the user to answer a
    /// permission prompt, confirmation, choice, or pinentry. Non-terminal: the
    /// task resumes running once the input is provided.
    AwaitingInput,
    /// Completed successfully.
    Succeeded,
    /// Terminated with an error.
    Failed,
    /// Cancelled before completion.
    Cancelled,
}

impl TaskStatus {
    /// Stable string form used for persistence and on-the-wire encoding.
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::Running => "running",
            TaskStatus::AwaitingInput => "awaiting_input",
            TaskStatus::Succeeded => "succeeded",
            TaskStatus::Failed => "failed",
            TaskStatus::Cancelled => "cancelled",
        }
    }
}

impl FromStr for TaskStatus {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "pending" => TaskStatus::Pending,
            "running" => TaskStatus::Running,
            "awaiting_input" => TaskStatus::AwaitingInput,
            "succeeded" => TaskStatus::Succeeded,
            "failed" => TaskStatus::Failed,
            "cancelled" => TaskStatus::Cancelled,
            _ => return Err(()),
        })
    }
}

/// Lifecycle of a single execution attempt of a task.
///
/// A [`Task`] is the logical work item; each time the daemon runs it, a new
/// [`TaskRun`] records that attempt. `RunStatus` mirrors [`TaskStatus`] but adds
/// the run-only outcomes a daemon restart can produce (`interrupted`) and the
/// deadline case (`timed_out`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Created, not yet picked up by the agent runtime.
    Pending,
    /// Currently being executed.
    Running,
    /// Running, but the agent is blocked waiting for user input.
    AwaitingInput,
    /// The attempt completed successfully.
    Succeeded,
    /// The attempt failed.
    Failed,
    /// The attempt was cancelled before completion.
    Cancelled,
    /// The daemon/supervisor lost the attempt (e.g. restart reconciliation).
    Interrupted,
    /// The attempt exceeded its deadline.
    TimedOut,
}

impl RunStatus {
    /// Stable string form used for persistence and on-the-wire encoding.
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Pending => "pending",
            RunStatus::Running => "running",
            RunStatus::AwaitingInput => "awaiting_input",
            RunStatus::Succeeded => "succeeded",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
            RunStatus::Interrupted => "interrupted",
            RunStatus::TimedOut => "timed_out",
        }
    }

    /// Whether the run is finished and will not change again.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            RunStatus::Succeeded
                | RunStatus::Failed
                | RunStatus::Cancelled
                | RunStatus::Interrupted
                | RunStatus::TimedOut
        )
    }
}

impl FromStr for RunStatus {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "pending" => RunStatus::Pending,
            "running" => RunStatus::Running,
            "awaiting_input" => RunStatus::AwaitingInput,
            "succeeded" => RunStatus::Succeeded,
            "failed" => RunStatus::Failed,
            "cancelled" => RunStatus::Cancelled,
            "interrupted" => RunStatus::Interrupted,
            "timed_out" => RunStatus::TimedOut,
            _ => return Err(()),
        })
    }
}

/// One execution attempt of a [`Task`]. The task is the logical work item; a run
/// is one attempt at it. Run rows are history/metrics/reconciliation records,
/// not a second source of truth for the task's live state.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TaskRun {
    pub id: Uuid,
    pub task_id: Uuid,
    /// 1-based attempt counter within the task.
    pub attempt: u32,
    pub status: RunStatus,
    /// Name of the agent that ran (or was configured for) this attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The provider/model the attempt was configured with (e.g. the task's
    /// `model`), so usage can be attributed. Absent for runs that predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The agent's own session id for this attempt, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Token/cost usage reported by the agent for this attempt. Stored on the
    /// run (not inside `tasks.output`) so it survives the retention pass that
    /// clears output blobs and can be aggregated by `usage.stats`. Absent when
    /// the agent reported no usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<AgentUsage>,
    /// Machine-readable classification, defined by #141. Absent while the run is
    /// still in flight or when it did not fail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
}

/// Why a task ended in failure, so a controller can distinguish a genuine
/// agent failure from an infrastructure fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// The agent ran and reported failure (non-zero exit, rejected work, …).
    Agent,
    /// The daemon could not start or supervise the run (spawn/PTY/daemon fault).
    Infrastructure,
    /// The run exceeded its deadline.
    Timeout,
    /// Input was invalid (missing required vars, unparseable manifest, …).
    InvalidInput,
    /// A dependency could not be satisfied.
    Dependency,
    /// Cancelled by a user or controller.
    Cancelled,
    /// Cannot proceed until something external changes; terminal and never
    /// retried automatically.
    Blocked,
    /// Unclassified failure.
    Unknown,
}

impl FailureKind {
    /// Whether an unattended retry could plausibly help for this kind. Mirrors
    /// the design's default `retry_on = ["infrastructure", "timeout"]`; producers
    /// may still override the flag per [`Failure`].
    pub fn retryable_by_default(self) -> bool {
        matches!(self, FailureKind::Infrastructure | FailureKind::Timeout)
    }
}

/// A machine-readable task failure. `Task.error` remains the human-readable
/// view; this is what controllers branch on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Failure {
    pub kind: FailureKind,
    pub message: String,
    /// Whether an unattended retry could plausibly help.
    pub retryable: bool,
}

impl Failure {
    /// Build a failure whose `retryable` flag follows the kind's default.
    pub fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retryable: kind.retryable_by_default(),
        }
    }
}

/// A unit of work the favetto is driving.
///
/// Tasks are created by integrations, hooks, the scheduler, or manually via the
/// API, then handed to the executor which runs the referenced catalog task through
/// its external agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Task {
    pub id: Uuid,
    /// Name of the catalog task (the `*.md` file) that defines how to run this.
    pub name: String,
    pub status: TaskStatus,
    /// How many execution attempts this task has had. Starts at 0; each claim
    /// increments it and records a [`TaskRun`] under the new attempt. Defaults to
    /// 0 so rows and payloads written before attempts existed decode unchanged.
    #[serde(default)]
    pub attempt: u32,
    /// Arbitrary input passed to the task's prompt.
    pub input: serde_json::Value,
    /// Produced by the task on completion. Omitted from list and push payloads;
    /// fetch it on demand with `tasks.get`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<serde_json::Value>,
    /// Optional dedupe key. Inserting a second task with the same key is a no-op.
    pub dedupe_key: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    /// Human-readable failure reason when `status == Failed`.
    pub error: Option<String>,
    /// Machine-readable failure classification when `status == Failed`. Absent on
    /// success and on rows/payloads written before typed failures existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
    /// The agent's own session id (e.g. opencode's session id), captured from a
    /// task run's output so the session can be reattached later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The agent's human-readable session title (e.g. opencode's generated title),
    /// resolved once at run completion from the agent's session store. Absent until
    /// a run finishes or when the agent reports no title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_title: Option<String>,
    /// The task that directly enqueued this one (a `spawn` parent, a `needs`
    /// predecessor, …). `None` for a task started directly (manual, scheduled,
    /// RPC, hook, webhook).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<Uuid>,
    /// The workflow origin this run belongs to. A directly-started task has no
    /// root of its own (it *is* the root, so [`Task::root_or_self`] falls back to
    /// its id); a spawned child inherits its parent's root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_id: Option<Uuid>,
    /// True when the run was started by a user and should execute in the agent's
    /// interactive TUI (the Agent panel attaches to it live). Programmatic starts
    /// (scheduler, webhooks, hooks, `needs`, `spawn`) leave this false and run
    /// headless. Defaults to false so older payloads and rows decode unchanged.
    #[serde(default)]
    pub interactive: bool,
    /// Optional per-launch agent override. A manual launch can run the task with
    /// a different configured agent than the catalog definition pins. The
    /// executor resolves it before the definition's `agent` and the global
    /// `[agent].default`. Absent for tasks that use the definition default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Optional per-launch model provider override. Resolved atomically with
    /// [`Self::model`]: an override of either field never mixes with the
    /// definition's provider/model pair.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Optional per-launch model id override (a bare id or `provider/model`).
    /// Absent for tasks that use the definition's or the agent's default model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl Task {
    /// Wrap this task as a server push so the daemon can broadcast it to TUI clients.
    ///
    /// `favetto-core` cannot log, so serialization failures are returned instead
    /// of being turned into an empty payload; the caller logs and skips the push.
    pub fn into_notification(self) -> Result<Notification, serde_json::Error> {
        Ok(Notification {
            method: push::TASK_UPDATED.to_string(),
            params: serde_json::to_value(self)?,
        })
    }

    /// A copy without the (potentially large) `output` blob. Used by list and
    /// push payloads, which never carry output.
    pub fn summary(&self) -> Task {
        let mut task = self.clone();
        task.output = None;
        task
    }

    /// The workflow root this run belongs to: its `root_id` when set, else its
    /// own id (a directly-started task is its own root).
    pub fn root_or_self(&self) -> Uuid {
        self.root_id.unwrap_or(self.id)
    }
}

/// Kinds of events emitted on the event bus.
///
/// Task and cron events are produced by the daemon's own lifecycle (the executor,
/// the scheduler and the RPC surface); integration events (`IssueCreated`,
/// `PrMerged`, `PushReceived`, ...) are emitted by webhook receivers as GitHub
/// (or other provider) activity arrives.
///
/// The macro below is the single source of truth. The enum variants,
/// [`EventKind::ALL`], [`EventKind::as_str`], [`EventKind::description`] and
/// [`EventKind::from_name`] are all generated from the same table, so adding a
/// kind is a one-line edit. Each entry carries its `snake_case` wire/storage
/// name, the spec's `PascalCase` spelling and a one-line description.
macro_rules! event_kinds {
    ($(
        $(#[$meta:meta])*
        $variant:ident => $snake:literal, $pascal:literal, $description:literal;
    )*) => {
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
        #[serde(rename_all = "snake_case")]
        pub enum EventKind {
            $(
                $(#[$meta])*
                $variant,
            )*
        }

        /// The event-kind table: `(kind, snake_case, PascalCase, description)`.
        ///
        /// Kept as the single source of truth for the generated methods below.
        pub const KINDS: &[(EventKind, &str, &str, &str)] = &[
            $( (EventKind::$variant, $snake, $pascal, $description), )*
        ];

        impl EventKind {
            /// Every [`EventKind`], in declaration order.
            pub const ALL: &'static [EventKind] = &[
                $( EventKind::$variant, )*
            ];

            /// Stable string form used for persistence and on-the-wire encoding.
            pub fn as_str(&self) -> &'static str {
                match self {
                    $( EventKind::$variant => $snake, )*
                }
            }

            /// A one-line explanation of when this event is emitted, used by the
            /// generated event-kinds reference.
            pub fn description(&self) -> &'static str {
                match self {
                    $( EventKind::$variant => $description, )*
                }
            }

            /// Parse an event name in either the wire `snake_case` form or the
            /// spec's `PascalCase` form (e.g. `"ticket_created"` or
            /// `"TicketCreated"`).
            pub fn from_name(s: &str) -> Option<Self> {
                match s {
                    $( $snake | $pascal => Some(EventKind::$variant), )*
                    // Rows written before `Synthetic` was renamed to `Unknown`.
                    "synthetic" | "Synthetic" => Some(EventKind::Unknown),
                    _ => None,
                }
            }
        }
    };
}

event_kinds! {
    TaskCreated => "task_created", "TaskCreated", "A task row was created.";
    TaskUpdated => "task_updated", "TaskUpdated", "A task row changed.";
    TaskCompleted => "task_completed", "TaskCompleted", "A task completed successfully (terminal).";
    TaskFailed => "task_failed", "TaskFailed", "A task failed (terminal).";
    TaskCancelled => "task_cancelled", "TaskCancelled", "A task was cancelled (terminal).";
    /// A task was enqueued and is idle (waiting to run).
    TaskIdle => "task_idle", "TaskIdle", "A task was enqueued and is waiting to run.";
    /// A task began running.
    TaskStarted => "task_started", "TaskStarted", "A task began running.";
    /// A running task's agent is blocked waiting for user input.
    TaskAwaitingInput => "task_awaiting_input", "TaskAwaitingInput", "A running task's agent is blocked waiting for user input.";
    /// A task ended (success or failure) — used by `needs` dependencies.
    TaskFinished => "task_finished", "TaskFinished", "A task ended (success or failure); the payload carries `name`, `task_id`, `success`, `status`, `attempt`, `retryable`, and an optional bounded `summary`; used by `needs` dependencies.";
    CronTick => "cron_tick", "CronTick", "A scheduled cron job fired.";
    IssueCreated => "issue_created", "IssueCreated", "An issue was created.";
    IssueUpdated => "issue_updated", "IssueUpdated", "An issue was edited.";
    IssueClosed => "issue_closed", "IssueClosed", "An issue was closed.";
    IssueReopened => "issue_reopened", "IssueReopened", "An issue was reopened.";
    IssueLabeled => "issue_labeled", "IssueLabeled", "A label was added to or removed from an issue.";
    IssueAssigned => "issue_assigned", "IssueAssigned", "An issue was assigned.";
    IssueCommentCreated => "issue_comment_created", "IssueCommentCreated", "A comment was posted on an issue.";
    PrCreated => "pr_created", "PRCreated", "A pull request was opened.";
    PrMerged => "pr_merged", "PRMerged", "A pull request was merged.";
    PrClosed => "pr_closed", "PRClosed", "A pull request was closed without merging.";
    PrReopened => "pr_reopened", "PRReopened", "A pull request was reopened.";
    PrSynchronized => "pr_synchronized", "PRSynchronized", "A pull request received new commits.";
    PrReadyForReview => "pr_ready_for_review", "PRReadyForReview", "A draft pull request was marked ready for review.";
    PrReviewRequested => "pr_review_requested", "PRReviewRequested", "A review was requested on a pull request.";
    PrUpdated => "pr_updated", "PRUpdated", "A pull request was edited.";
    PrReviewSubmitted => "pr_review_submitted", "PRReviewSubmitted", "A review was submitted on a pull request.";
    PushReceived => "push_received", "PushReceived", "A push was received on a branch.";
    TicketCreated => "ticket_created", "TicketCreated", "A ticket was created (integration event).";
    TicketUpdated => "ticket_updated", "TicketUpdated", "A ticket was updated (integration event).";
    ActionRunCompleted => "action_run_completed", "ActionRunCompleted", "A workflow run completed.";
    CheckSuiteCompleted => "check_suite_completed", "CheckSuiteCompleted", "A check suite completed.";
    CheckRunCompleted => "check_run_completed", "CheckRunCompleted", "A check run completed.";
    EmailReceived => "email_received", "EmailReceived", "An email was received (integration event).";
    /// Fallback for a persisted event whose kind is not recognised.
    Unknown => "unknown", "Unknown", "An unrecognised persisted event kind (decode fallback).";
}

impl FromStr for EventKind {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_name(s).ok_or(())
    }
}

/// An immutable record of something that happened.
///
/// `id` is a monotonically increasing integer (SQLite `INTEGER PRIMARY KEY`
/// AUTOINCREMENT) and doubles as the resume cursor for remote TUI subscriptions:
/// a client reconnecting with `last_event_id` replays everything after it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Event {
    pub id: i64,
    pub kind: EventKind,
    pub payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

impl Event {
    /// Wrap this event as a server push so the daemon can broadcast it to TUI clients.
    ///
    /// `favetto-core` cannot log, so serialization failures are returned instead
    /// of being turned into an empty payload; the caller logs and skips the push.
    pub fn into_notification(self) -> Result<Notification, serde_json::Error> {
        Ok(Notification {
            method: push::EVENT.to_string(),
            params: serde_json::to_value(self)?,
        })
    }
}

/// Role of a [`ChatMessage`] in an agent conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

impl MessageRole {
    /// Stable string form used for persistence and on-the-wire encoding.
    pub fn as_str(self) -> &'static str {
        match self {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        }
    }
}

impl std::str::FromStr for MessageRole {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "system" => MessageRole::System,
            "user" => MessageRole::User,
            "assistant" => MessageRole::Assistant,
            "tool" => MessageRole::Tool,
            _ => return Err(()),
        })
    }
}

/// A single turn in an agent conversation (the LLM chat).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub content: String,
    /// Optional chain-of-thought produced by the model before the content. This is
    /// display-only: it is never sent back to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

impl ChatMessage {
    pub fn new(role: MessageRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            reasoning: None,
        }
    }

    pub fn with_reasoning(mut self, reasoning: Option<String>) -> Self {
        self.reasoning = reasoning;
        self
    }
}

/// What kind of decision an agent is blocked on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AwaitingInputKind {
    /// A tool/command permission prompt.
    Permission,
    /// A yes/no confirmation.
    Confirmation,
    /// A multiple-choice selection.
    Choice,
    /// A passphrase/pinentry prompt (e.g. `git` signing).
    Pinentry,
    /// An ACP v1 form elicitation request.
    Elicitation,
    /// A prompt was detected but could not be classified further.
    Other,
}

/// Why a session is considered blocked on the user.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AwaitingInputReason {
    pub kind: AwaitingInputKind,
    /// The prompt text (the last visible lines), for context.
    pub message: String,
    /// Transport-specific correlation id, when the prompt can be answered
    /// through a structured channel (`agents.reply`). Absent for screen-detected
    /// prompts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Selectable options the prompt offers; empty means free-form text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// Whether an "always" / "remember" answer is offered.
    #[serde(default, skip_serializing_if = "crate::agent_state::is_false")]
    pub allow_always: bool,
    /// The pending form for a form elicitation request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form: Option<crate::elicitation::ElicitationForm>,
}

/// How an agent session is driven.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AgentBackend {
    /// A CLI running in a PTY, observed through its rendered screen.
    #[default]
    Pty,
    /// An ACP server driven over JSON-RPC (structured messages and tool calls).
    Acp,
}

/// How a session answers tool-permission requests raised by its agent.
///
/// The policy belongs to the task definition (or the agent entry as a default),
/// so an unattended run can choose `deny` while an attended run asks the user.
/// Agents that never ask (because their own configuration auto-approves) are
/// unaffected: the policy only decides how a request that *does* arrive is
/// answered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PermissionPolicy {
    /// Forward the request to a connected client and wait for its answer. With
    /// no client attached, the request fails closed after a timeout.
    #[default]
    Ask,
    /// Approve the request using the agent's `allow_once` option.
    Allow,
    /// Reject every request.
    Deny,
}

/// Whether a session's ACP conversation is available yet.
///
/// A newly created session is [`Ready`](AgentLoadState::Ready) once its process
/// exists. A restored session starts [`Loading`](AgentLoadState::Loading): the
/// daemon does not publish its replay as ready until `session/load` succeeds.
/// A failed restore is terminal for that attempt and never reported as ready.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AgentLoadState {
    /// The conversation is live or was newly created; history is available.
    #[default]
    Ready,
    /// A persisted conversation is being restored; its replay is not ready yet.
    Loading,
    /// The restore failed; no partial conversation is exposed as restored.
    Failed,
}

/// How an advertised ACP v1 authentication method is handled.
///
/// Favetto is a daemon/remote-client architecture: a terminal method would have
/// to run an interactive agent process on the daemon host, so it is preserved
/// for display but never offered as a usable action.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AgentAuthMethodKind {
    /// The agent handles authentication itself through ACP `authenticate`.
    #[default]
    Agent,
    /// The client would have to launch an interactive terminal process.
    Terminal,
}

/// One authentication method advertised by an ACP agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentAuthMethod {
    /// The method id passed back to ACP `authenticate`.
    pub id: String,
    /// Human-readable method name.
    pub name: String,
    /// Optional human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether this is an agent-handled or terminal method.
    #[serde(default)]
    pub kind: AgentAuthMethodKind,
    /// Whether the daemon can carry out this method for a remote client.
    ///
    /// This is `false` for terminal methods until a safe interactive bridge
    /// exists. Clients must not present an unsupported method as actionable.
    #[serde(default)]
    pub supported: bool,
}

/// Where a session's ACP authentication currently stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AgentAuthStatus {
    /// The agent did not require authentication (or a sign-out completed).
    #[default]
    NotRequired,
    /// Session setup is paused until the user completes a sign-in.
    Required,
    /// An `authenticate` request is in flight.
    Authenticating,
    /// Authentication succeeded; session setup is being retried.
    Authenticated,
    /// The last sign-in attempt failed. The session remains retryable.
    Failed,
}

/// Session-scoped ACP authentication state exposed to daemon clients.
///
/// It deliberately contains only method metadata, status and a safe message: no
/// credential, token, or agent-authored terminal output ever enters this state
/// or the MessagePack payloads built from it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentAuthState {
    #[serde(default)]
    pub status: AgentAuthStatus,
    /// Methods advertised during ACP `initialize`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<AgentAuthMethod>,
    /// Whether the agent advertised ACP `logout`.
    #[serde(default, skip_serializing_if = "crate::agent_state::is_false")]
    pub logout_supported: bool,
    /// The agent-handled method currently or most recently attempted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method_id: Option<String>,
    /// Safe progress/error text; never a credential or raw terminal sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// A live external-agent session on the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentSessionInfo {
    pub id: String,
    /// Name of the `[agents.*]` entry that launched it.
    pub agent: String,
    /// Whether this session is PTY-backed or ACP-backed. Clients use it to pick
    /// between terminal-frame rendering and structured event rendering.
    #[serde(default)]
    pub backend: AgentBackend,
    /// Catalog task this session is attached to, if any.
    pub task_id: Option<String>,
    /// Whether the child process is still running.
    pub running: bool,
    /// Whether this is an unattended run (machine-readable output) rather than an
    /// interactive TUI that can be attached.
    #[serde(default)]
    pub headless: bool,
    /// The agent's own session id (e.g. opencode's), captured from its output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Set while the session is blocked waiting for the user to answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub awaiting_input: Option<AwaitingInputReason>,
    /// The agent's current coarse activity, when a state channel reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<AgentActivity>,
    /// Token/cost usage reported for this session so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<AgentUsage>,
    /// Whether this session's conversation is still being restored. Additive on
    /// the wire: older payloads default to `ready`, and older clients ignore it.
    #[serde(default)]
    pub load_state: AgentLoadState,
    /// Why a restore failed, when `load_state` is `failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_error: Option<String>,
    /// The complete ACP session config-option snapshot advertised by the agent.
    /// Replaced wholesale by `config_option_update`; an empty list means the
    /// live session offers no general options.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_options: Vec<SessionConfigOption>,
    /// The session's mode state, when the agent advertises modes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modes: Option<SessionModes>,
    /// Session-scoped ACP authentication state, when the agent advertised any
    /// methods or required sign-in. Absent for PTY sessions and agents that
    /// neither advertise auth methods nor logout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AgentAuthState>,
}

/// The lifecycle of a persisted ACP session, as shown by `agents.history`.
///
/// This is display state, not a routing key. The Favetto-local [`id`] is the
/// only id a client sends back to `agents.load`; the agent's own ACP session id
/// stays private to the daemon.
///
/// [`id`]: AgentSessionHistoryEntry::id
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionState {
    /// A live ACP process is attached to this session.
    #[default]
    Active,
    /// A persisted conversation is being restored (`session/load` in flight).
    Loading,
    /// The session has ended (or outlived a daemon restart) and may be loaded.
    Ended,
    /// The launch, load, or the agent itself failed.
    Failed,
}

/// One Favetto-created ACP session in the durable history, as returned by
/// `agents.history`.
///
/// The id is Favetto's local routing key and is deliberately not the agent's
/// ACP session id. A row is only `loadable` when the daemon recorded the ACP id
/// needed for `session/load` and the owning agent is still configured and
/// available. When it is not loadable, `load_error` explains why (an
/// unsupported/expired session, or a recorded failure).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentSessionHistoryEntry {
    /// Favetto-local id; the only routing key accepted by `agents.load`.
    pub id: String,
    /// Configured `[agents.<name>]` key that owns the session.
    pub agent: String,
    /// Display title reported by the agent, when it reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Working directory the session was created in.
    pub cwd: PathBuf,
    /// Catalog task this session belongs to, if task-backed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Current lifecycle/load state.
    pub state: AgentSessionState,
    /// Whether `agents.load` can restore this session right now.
    #[serde(default)]
    pub loadable: bool,
    /// Why the session cannot be loaded, or the last restore/launch failure.
    ///
    /// `None` for a loadable session and for an active/live row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_error: Option<String>,
    /// When the session record was created.
    pub created_at: DateTime<Utc>,
    /// When the record last changed (a load or close updates it).
    pub updated_at: DateTime<Utc>,
}

/// One provider-owned (ACP) session discovered through `session/list`.
///
/// Provider history is deliberately separate from Favetto's durable
/// [`AgentSessionHistoryEntry`] records: a provider row may never have been seen
/// by Favetto. `session_id` is the provider's own ACP id and is only accepted by
/// the provider-specific RPCs (`agents.provider_resume` /
/// `agents.provider_delete`), never by `agents.load` or `agents.attach`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProviderSessionEntry {
    /// Configured `[agents.<name>]` key that owns the provider session.
    pub agent: String,
    /// The agent's ACP session id.
    pub session_id: String,
    /// Working directory reported by the provider.
    pub cwd: PathBuf,
    /// Display title reported by the provider, when it reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Provider-reported last-activity timestamp (ISO 8601), when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    /// The Favetto-local record already imported for this provider session, if
    /// any. When present, resume reuses that local row instead of creating a
    /// duplicate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_session_id: Option<String>,
}

/// One configured agent's provider-session page, as returned by
/// `agents.provider_sessions`.
///
/// `supported` is the `sessionCapabilities.list` advertisement. `sessions` is
/// empty when list is unsupported or the provider failed; `error` explains why.
/// `can_resume` covers `loadSession` or `sessionCapabilities.resume`, and
/// `can_delete` covers `sessionCapabilities.delete`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProviderSessionsResult {
    /// Configured agent key these sessions belong to.
    pub agent: String,
    /// Whether the agent advertised ACP `session/list`.
    #[serde(default)]
    pub supported: bool,
    /// Whether provider sessions can be restored (`session/load` or
    /// `session/resume`).
    #[serde(default)]
    pub can_resume: bool,
    /// Whether the agent advertised ACP `session/delete`.
    #[serde(default)]
    pub can_delete: bool,
    /// This page's provider sessions.
    #[serde(default)]
    pub sessions: Vec<ProviderSessionEntry>,
    /// Opaque cursor for the next page, when the provider reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Why listing failed or is unsupported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What an external agent CLI can do, so the daemon and TUI can adapt without
/// hard-coding a specific CLI's behaviour.
///
/// Every field defaults to `false` and is additive on the wire: an entry written
/// by an older version decodes with all capabilities off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentCapabilities {
    /// Can run an attachable interactive TUI (the Agent panel).
    #[serde(default)]
    pub interactive: bool,
    /// Speaks the Agent Client Protocol, so prompts, tool calls and permissions
    /// arrive as structured events instead of a terminal screen.
    #[serde(default)]
    pub acp: bool,
    /// Can run a task unattended (non-interactively).
    #[serde(default)]
    pub headless: bool,
    /// Can reopen a previous session (`resume_args` / an agent session id).
    #[serde(default)]
    pub resume: bool,
    /// Accepts a provider/model selection.
    #[serde(default)]
    pub model_selection: bool,
    /// Exposes a provider/model catalog (`providers.list`).
    #[serde(default)]
    pub providers: bool,
    /// Produces structured (machine-parseable) output.
    #[serde(default)]
    pub structured_output: bool,
    /// Reports its own session id in the output.
    #[serde(default)]
    pub reports_session_id: bool,
    /// Seeds, but does not submit, a prompt (needs an Enter press).
    #[serde(default)]
    pub prompt_prefill: bool,
    /// Can be seeded with a prompt in interactive mode (`prompt_args` and/or
    /// `submit_prompt`), so a user-started task can run in the real TUI.
    #[serde(default)]
    pub interactive_prompt: bool,
    /// Reports live state through a structured channel.
    #[serde(default)]
    pub reports_state: bool,
    /// Can answer permission/dialog prompts through the channel.
    #[serde(default)]
    pub permission_channel: bool,
    /// Can open a second, interactive session attached to an already-running
    /// headless run without racing its session file (e.g. opencode's managed
    /// server), so the Agent panel can show the real TUI concurrently with the
    /// run instead of its machine output.
    #[serde(default)]
    pub concurrent_attach: bool,
}

/// Default for a payload that omits `available`: older daemons/clients only know
/// about agents they can actually launch, so treat them as available.
fn default_available() -> bool {
    true
}

/// A configured external agent plus its live-session state, as returned by
/// `agents.list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentCatalogEntry {
    /// Configured/built-in key, e.g. `opencode`.
    pub name: String,
    /// Human-readable CLI name, e.g. `OpenCode`.
    #[serde(default)]
    pub display_name: String,
    pub command: String,
    pub default: bool,
    /// Whether the agent's executable was found on the daemon's PATH.
    #[serde(default = "default_available")]
    pub available: bool,
    /// What the agent can do; see [`AgentCapabilities`].
    #[serde(default)]
    pub capabilities: AgentCapabilities,
    /// Live sessions launched from this agent.
    pub sessions: Vec<AgentSessionInfo>,
}

/// A cron schedule that enqueues a task (and emits a `CronTick`) on fire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Schedule {
    pub id: String,
    pub cron: String,
    /// The catalog task name to enqueue on each fire.
    pub task: String,
    pub input: serde_json::Value,
    pub enabled: bool,
    pub last_run: Option<DateTime<Utc>>,
    /// The next occurrence of `cron` in UTC, computed by the daemon for enabled
    /// schedules. `None` when the schedule is disabled, its expression does not
    /// parse, or no future occurrence exists. Never persisted; older payloads
    /// omit it.
    #[serde(default)]
    pub next_run: Option<DateTime<Utc>>,
}

/// How a repository entered the known-repos registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum RepoSource {
    /// Registered explicitly: a `[[repos.known]]` seed or a `repos.add` call.
    #[default]
    Manual,
    /// Discovered by scanning `[repos].base_dir` for `.favetto/` directories.
    Scanned,
}

/// A repository known to the daemon, as returned by `repos.list`.
///
/// `name` is the namespace key that qualifies the repo's tasks
/// (`repo:<name>/<path>`); `path` is the absolute daemon-host directory. A repo
/// is `available` when its `.favetto/tasks/` directory exists, so a moved or
/// unmounted checkout stays listed but contributes no tasks. `source` records how
/// the repo entered the registry: explicitly (`manual`) or via the base-directory
/// scan (`scanned`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RepoInfo {
    pub name: String,
    pub path: String,
    /// Whether the repo currently has a `.favetto/tasks/` catalog directory.
    #[serde(default = "default_available")]
    pub available: bool,
    /// Whether this is the daemon-wide active repo.
    #[serde(default)]
    pub active: bool,
    /// How the repo entered the registry.
    #[serde(default)]
    pub source: RepoSource,
}

/// A persisted notification record (sent history).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NotificationRecord {
    pub id: i64,
    pub channel: String,
    pub subject: String,
    pub body: String,
    pub status: String,
    pub sent_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_session_history_round_trips_and_omits_the_provider_id() {
        let entry = AgentSessionHistoryEntry {
            id: "local-1".to_string(),
            agent: "opencode".to_string(),
            title: Some("Fix the widget".to_string()),
            cwd: PathBuf::from("/work"),
            task_id: Some("task-1".to_string()),
            state: AgentSessionState::Ended,
            loadable: true,
            load_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["id"], "local-1");
        assert_eq!(json["state"], "ended");
        assert_eq!(json["loadable"], true);
        assert!(json.get("load_error").is_none());
        // Only the Favetto-local id is present; there is no provider-id field.
        assert!(json.get("session_id").is_none());
        let back: AgentSessionHistoryEntry = serde_json::from_value(json).unwrap();
        assert_eq!(back, entry);

        // Additive fields default on a lean payload from a newer/older producer.
        let lean: AgentSessionHistoryEntry = serde_json::from_str(
            r#"{
                "id": "local-2",
                "agent": "pi",
                "cwd": "/work",
                "state": "failed",
                "created_at": "2024-01-01T00:00:00Z",
                "updated_at": "2024-01-01T00:00:00Z"
            }"#,
        )
        .unwrap();
        assert_eq!(lean.title, None);
        assert_eq!(lean.task_id, None);
        assert!(!lean.loadable);
        assert_eq!(lean.load_error, None);
    }

    #[test]
    fn agent_catalog_entry_round_trips_capabilities() {
        let entry = AgentCatalogEntry {
            name: "opencode".to_string(),
            display_name: "OpenCode".to_string(),
            command: "opencode".to_string(),
            default: true,
            available: true,
            capabilities: AgentCapabilities {
                interactive: true,
                acp: false,
                headless: true,
                resume: true,
                model_selection: true,
                providers: true,
                structured_output: true,
                reports_session_id: true,
                prompt_prefill: true,
                interactive_prompt: true,
                reports_state: true,
                permission_channel: true,
                concurrent_attach: true,
            },
            sessions: Vec::new(),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["capabilities"]["providers"], true);
        assert_eq!(json["available"], true);
        let back: AgentCatalogEntry = serde_json::from_value(json).unwrap();
        assert_eq!(back.name, entry.name);
        assert_eq!(back.display_name, entry.display_name);
        assert_eq!(back.capabilities, entry.capabilities);
        assert!(back.default);
        assert!(back.available);

        // Old payloads without the new fields still decode, defaulting them.
        let legacy: AgentCatalogEntry =
            serde_json::from_str(r#"{"name":"pi","command":"pi","default":false,"sessions":[]}"#)
                .unwrap();
        assert_eq!(legacy.display_name, "");
        assert_eq!(legacy.capabilities, AgentCapabilities::default());
        assert!(legacy.available, "omitted `available` must default to true");
    }

    #[test]
    fn task_session_title_round_trips_and_defaults() {
        let task = Task {
            id: Uuid::new_v4(),
            name: "t".to_string(),
            status: TaskStatus::Succeeded,
            attempt: 0,
            input: serde_json::json!({}),
            output: None,
            dedupe_key: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            error: None,
            failure: None,
            session_id: Some("ses_1".to_string()),
            session_title: Some("Fix the widget".to_string()),
            parent_id: None,
            root_id: None,
            interactive: false,
            agent: None,
            provider: None,
            model: None,
        };
        let json = serde_json::to_value(&task).unwrap();
        assert_eq!(json["session_title"], "Fix the widget");
        let back: Task = serde_json::from_value(json).unwrap();
        assert_eq!(back.session_title.as_deref(), Some("Fix the widget"));

        // Legacy payloads without `session_title` still decode, defaulting to None.
        let legacy: Task = serde_json::from_str(
            r#"{
                "id": "00000000-0000-0000-0000-000000000000",
                "name": "t",
                "status": "succeeded",
                "input": {},
                "output": null,
                "dedupe_key": null,
                "created_at": "2024-01-01T00:00:00Z",
                "started_at": null,
                "finished_at": null,
                "error": null
            }"#,
        )
        .unwrap();
        assert!(legacy.session_title.is_none());
        // Legacy payloads without lineage still decode and are their own root.
        assert!(legacy.parent_id.is_none());
        assert!(legacy.root_id.is_none());
        assert_eq!(legacy.root_or_self(), legacy.id);
    }

    #[test]
    fn task_attempt_round_trips_and_defaults_zero() {
        let task = Task {
            id: Uuid::new_v4(),
            name: "t".to_string(),
            status: TaskStatus::Running,
            attempt: 3,
            input: serde_json::json!({}),
            output: None,
            dedupe_key: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            error: None,
            failure: None,
            session_id: None,
            session_title: None,
            parent_id: None,
            root_id: None,
            interactive: false,
            agent: None,
            provider: None,
            model: None,
        };
        let json = serde_json::to_value(&task).unwrap();
        assert_eq!(json["attempt"], 3);
        let back: Task = serde_json::from_value(json).unwrap();
        assert_eq!(back.attempt, 3);

        // Legacy payloads without `attempt` decode as 0 attempts.
        let legacy: Task = serde_json::from_str(
            r#"{
                "id": "00000000-0000-0000-0000-000000000000",
                "name": "t",
                "status": "pending",
                "input": {},
                "dedupe_key": null,
                "created_at": "2024-01-01T00:00:00Z",
                "started_at": null,
                "finished_at": null,
                "error": null
            }"#,
        )
        .unwrap();
        assert_eq!(legacy.attempt, 0);
    }

    #[test]
    fn task_interactive_round_trips_and_defaults_false() {
        let task = Task {
            id: Uuid::new_v4(),
            name: "t".to_string(),
            status: TaskStatus::Pending,
            attempt: 0,
            input: serde_json::json!({}),
            output: None,
            dedupe_key: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            error: None,
            failure: None,
            session_id: None,
            session_title: None,
            parent_id: None,
            root_id: None,
            interactive: true,
            agent: None,
            provider: None,
            model: None,
        };
        let json = serde_json::to_value(&task).unwrap();
        assert_eq!(json["interactive"], true);
        let back: Task = serde_json::from_value(json).unwrap();
        assert!(back.interactive);

        // Legacy payloads without `interactive` decode as headless.
        let legacy: Task = serde_json::from_str(
            r#"{
                "id": "00000000-0000-0000-0000-000000000000",
                "name": "t",
                "status": "pending",
                "input": {},
                "dedupe_key": null,
                "created_at": "2024-01-01T00:00:00Z",
                "started_at": null,
                "finished_at": null,
                "error": null
            }"#,
        )
        .unwrap();
        assert!(!legacy.interactive);
    }

    #[test]
    fn task_lineage_round_trips() {
        let parent = Uuid::new_v4();
        let root = Uuid::new_v4();
        let task = Task {
            id: Uuid::new_v4(),
            name: "t".to_string(),
            status: TaskStatus::Succeeded,
            attempt: 0,
            input: serde_json::json!({}),
            output: None,
            dedupe_key: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            error: None,
            failure: None,
            session_id: None,
            session_title: None,
            parent_id: Some(parent),
            root_id: Some(root),
            interactive: false,
            agent: None,
            provider: None,
            model: None,
        };
        let json = serde_json::to_value(&task).unwrap();
        assert_eq!(json["parent_id"], parent.to_string());
        assert_eq!(json["root_id"], root.to_string());
        let back: Task = serde_json::from_value(json).unwrap();
        assert_eq!(back.parent_id, Some(parent));
        assert_eq!(back.root_id, Some(root));
        assert_eq!(back.root_or_self(), root);
    }

    #[test]
    fn task_output_omitted_decodes_and_summary_clears_it() {
        // A compact payload without an `output` key still decodes.
        let compact: Task = serde_json::from_str(
            r#"{
                "id": "00000000-0000-0000-0000-000000000000",
                "name": "t",
                "status": "succeeded",
                "input": {},
                "dedupe_key": null,
                "created_at": "2024-01-01T00:00:00Z",
                "started_at": null,
                "finished_at": null,
                "error": null
            }"#,
        )
        .unwrap();
        assert!(compact.output.is_none());

        // `summary()` drops the output blob but keeps the rest of the task.
        let task = Task {
            output: Some(serde_json::json!({ "huge": "x".repeat(10_000) })),
            ..compact
        };
        let summary = task.summary();
        assert!(summary.output.is_none());
        assert_eq!(summary.id, task.id);
        assert_eq!(summary.name, task.name);
        assert_eq!(summary.status, task.status);
    }

    #[test]
    fn event_kind_table_round_trips() {
        // The table is the single source of truth: every entry round-trips
        // through both spellings and its `snake_case` name matches `as_str`.
        for (kind, snake, pascal, description) in KINDS {
            assert_eq!(
                EventKind::from_name(snake),
                Some(kind.clone()),
                "from_name(snake) mismatch for {:?}",
                kind
            );
            assert_eq!(
                EventKind::from_name(pascal),
                Some(kind.clone()),
                "from_name(pascal) mismatch for {:?}",
                kind
            );
            assert_eq!(kind.as_str(), *snake, "as_str mismatch for {:?}", kind);
            assert!(
                !description.trim().is_empty(),
                "{kind:?} has an empty description"
            );
            assert!(
                EventKind::ALL.contains(kind),
                "{kind:?} is missing from EventKind::ALL"
            );
        }
    }

    #[test]
    fn all_covers_every_kind() {
        // Every `ALL` entry appears exactly once and has a unique name.
        assert_eq!(EventKind::ALL.len(), KINDS.len());
        let mut names: Vec<&str> = EventKind::ALL.iter().map(EventKind::as_str).collect();
        let len = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), len, "EventKind::ALL contains a duplicate");
    }

    #[test]
    fn unknown_kind_accepts_legacy_and_fallback_names() {
        // The pre-rename `Synthetic` spellings and the new name all map to Unknown.
        for name in ["unknown", "Unknown", "synthetic", "Synthetic"] {
            assert_eq!(
                EventKind::from_name(name),
                Some(EventKind::Unknown),
                "{name} should parse to Unknown"
            );
        }
        // Anything else is not an event kind.
        assert_eq!(EventKind::from_name("not_a_kind"), None);
        assert_eq!("not_a_kind".parse::<EventKind>(), Err(()));
    }

    #[test]
    fn descriptions_are_non_empty() {
        for kind in EventKind::ALL {
            assert!(
                !kind.description().trim().is_empty(),
                "{kind:?} has an empty description"
            );
        }
    }

    #[test]
    fn task_status_round_trips() {
        for status in [
            TaskStatus::Pending,
            TaskStatus::Running,
            TaskStatus::AwaitingInput,
            TaskStatus::Succeeded,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ] {
            assert_eq!(
                status.as_str().parse::<TaskStatus>(),
                Ok(status),
                "from_str/as_str mismatch for {status:?}"
            );
        }
        assert_eq!(TaskStatus::AwaitingInput.as_str(), "awaiting_input");
    }

    #[test]
    fn run_status_round_trips() {
        for status in [
            RunStatus::Pending,
            RunStatus::Running,
            RunStatus::AwaitingInput,
            RunStatus::Succeeded,
            RunStatus::Failed,
            RunStatus::Cancelled,
            RunStatus::Interrupted,
            RunStatus::TimedOut,
        ] {
            assert_eq!(
                status.as_str().parse::<RunStatus>(),
                Ok(status),
                "from_str/as_str mismatch for {status:?}"
            );
            let json = serde_json::to_value(status).unwrap();
            let back: RunStatus = serde_json::from_value(json).unwrap();
            assert_eq!(back, status);
        }
        // The wire spellings are stable.
        assert_eq!(RunStatus::AwaitingInput.as_str(), "awaiting_input");
        assert_eq!(RunStatus::TimedOut.as_str(), "timed_out");
        assert_eq!(RunStatus::Interrupted.as_str(), "interrupted");
    }

    #[test]
    fn run_status_is_terminal() {
        for status in [
            RunStatus::Succeeded,
            RunStatus::Failed,
            RunStatus::Cancelled,
            RunStatus::Interrupted,
            RunStatus::TimedOut,
        ] {
            assert!(status.is_terminal(), "{status:?} must be terminal");
        }
        for status in [
            RunStatus::Pending,
            RunStatus::Running,
            RunStatus::AwaitingInput,
        ] {
            assert!(!status.is_terminal(), "{status:?} must be non-terminal");
        }
    }

    #[test]
    fn task_run_round_trips() {
        let id = Uuid::new_v4();
        let task_id = Uuid::new_v4();
        let started_at = Utc::now();
        let finished_at = started_at + chrono::Duration::seconds(5);
        let run = TaskRun {
            id,
            task_id,
            attempt: 2,
            status: RunStatus::Failed,
            agent: Some("opencode".to_string()),
            model: Some("anthropic/claude-sonnet".to_string()),
            session_id: Some("ses_1".to_string()),
            started_at: Some(started_at),
            finished_at: Some(finished_at),
            exit_code: Some(1),
            error: Some("boom".to_string()),
            usage: Some(AgentUsage {
                input_tokens: 12,
                output_tokens: 3,
                cost_usd: Some(0.02),
                ..Default::default()
            }),
            failure: Some(Failure::new(FailureKind::Infrastructure, "boom")),
        };

        let json = serde_json::to_value(&run).unwrap();
        assert_eq!(json["id"], id.to_string());
        assert_eq!(json["task_id"], task_id.to_string());
        assert_eq!(json["status"], "failed");
        assert_eq!(json["attempt"], 2);
        assert_eq!(json["exit_code"], 1);
        assert_eq!(json["failure"]["kind"], "infrastructure");
        assert_eq!(json["failure"]["message"], "boom");
        assert_eq!(json["model"], "anthropic/claude-sonnet");
        assert_eq!(json["usage"]["input_tokens"], 12);
        assert_eq!(json["usage"]["cost_usd"], 0.02);

        let back: TaskRun = serde_json::from_value(json).unwrap();
        assert_eq!(back.id, run.id);
        assert_eq!(back.task_id, run.task_id);
        assert_eq!(back.attempt, run.attempt);
        assert_eq!(back.status, run.status);
        assert_eq!(back.agent, run.agent);
        assert_eq!(back.model, run.model);
        assert_eq!(back.session_id, run.session_id);
        assert_eq!(
            back.started_at.map(|t| t.timestamp_millis()),
            run.started_at.map(|t| t.timestamp_millis())
        );
        assert_eq!(
            back.finished_at.map(|t| t.timestamp_millis()),
            run.finished_at.map(|t| t.timestamp_millis())
        );
        assert_eq!(back.exit_code, run.exit_code);
        assert_eq!(back.error, run.error);
        assert_eq!(back.usage, run.usage);
        assert_eq!(back.failure, run.failure);
    }

    #[test]
    fn task_run_defaults_optional_fields() {
        // A minimal payload omitting every optional key still decodes.
        let legacy: TaskRun = serde_json::from_str(
            r#"{
                "id": "00000000-0000-0000-0000-000000000000",
                "task_id": "11111111-1111-1111-1111-111111111111",
                "attempt": 1,
                "status": "pending"
            }"#,
        )
        .unwrap();
        assert!(legacy.agent.is_none());
        assert!(legacy.model.is_none());
        assert!(legacy.session_id.is_none());
        assert!(legacy.started_at.is_none());
        assert!(legacy.finished_at.is_none());
        assert!(legacy.exit_code.is_none());
        assert!(legacy.error.is_none());
        assert!(legacy.usage.is_none());
        assert!(legacy.failure.is_none());
        // Absent optional fields are omitted from the serialized form.
        let json = serde_json::to_value(&legacy).unwrap();
        assert!(json.get("agent").is_none());
        assert!(json.get("model").is_none());
        assert!(json.get("usage").is_none());
        assert!(json.get("failure").is_none());
    }

    #[test]
    fn failure_kind_round_trips() {
        for kind in [
            FailureKind::Agent,
            FailureKind::Infrastructure,
            FailureKind::Timeout,
            FailureKind::InvalidInput,
            FailureKind::Dependency,
            FailureKind::Cancelled,
            FailureKind::Blocked,
            FailureKind::Unknown,
        ] {
            let json = serde_json::to_value(kind).unwrap();
            let back: FailureKind = serde_json::from_value(json).unwrap();
            assert_eq!(back, kind);
        }
        assert_eq!(
            serde_json::to_value(FailureKind::InvalidInput).unwrap(),
            "invalid_input"
        );
    }

    #[test]
    fn failure_round_trips() {
        let failure = Failure::new(FailureKind::Infrastructure, "boom");
        let json = serde_json::to_value(&failure).unwrap();
        assert_eq!(json["kind"], "infrastructure");
        assert_eq!(json["message"], "boom");
        assert_eq!(json["retryable"], true);
        let back: Failure = serde_json::from_value(json).unwrap();
        assert_eq!(back, failure);
    }

    #[test]
    fn failure_retryable_defaults_only_for_infrastructure_and_timeout() {
        assert!(FailureKind::Infrastructure.retryable_by_default());
        assert!(FailureKind::Timeout.retryable_by_default());
        for kind in [
            FailureKind::Agent,
            FailureKind::InvalidInput,
            FailureKind::Dependency,
            FailureKind::Cancelled,
            FailureKind::Blocked,
            FailureKind::Unknown,
        ] {
            assert!(!kind.retryable_by_default(), "{kind:?} must not retry");
        }
    }

    #[test]
    fn task_failure_round_trips_and_defaults() {
        let task = Task {
            id: Uuid::new_v4(),
            name: "t".to_string(),
            status: TaskStatus::Failed,
            attempt: 0,
            input: serde_json::json!({}),
            output: None,
            dedupe_key: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            error: Some("exit 1".to_string()),
            failure: Some(Failure::new(FailureKind::Agent, "exit 1")),
            session_id: None,
            session_title: None,
            parent_id: None,
            root_id: None,
            interactive: false,
            agent: None,
            provider: None,
            model: None,
        };
        let json = serde_json::to_value(&task).unwrap();
        assert_eq!(json["failure"]["kind"], "agent");
        assert_eq!(json["failure"]["retryable"], false);
        assert_eq!(json["error"], "exit 1");
        let back: Task = serde_json::from_value(json).unwrap();
        assert_eq!(back.failure, task.failure);

        // Legacy payloads without `failure` still decode, keeping `error`.
        let legacy: Task = serde_json::from_str(
            r#"{
                "id": "00000000-0000-0000-0000-000000000000",
                "name": "t",
                "status": "failed",
                "input": {},
                "dedupe_key": null,
                "created_at": "2024-01-01T00:00:00Z",
                "started_at": null,
                "finished_at": null,
                "error": "legacy boom"
            }"#,
        )
        .unwrap();
        assert!(legacy.failure.is_none());
        assert_eq!(legacy.error.as_deref(), Some("legacy boom"));
    }

    #[test]
    fn awaiting_input_reason_round_trips() {
        for kind in [
            AwaitingInputKind::Permission,
            AwaitingInputKind::Confirmation,
            AwaitingInputKind::Choice,
            AwaitingInputKind::Pinentry,
            AwaitingInputKind::Elicitation,
            AwaitingInputKind::Other,
        ] {
            let reason = AwaitingInputReason {
                kind,
                message: "Allow once / Allow always / Reject".to_string(),
                request_id: Some("perm_1".to_string()),
                options: vec!["Allow once".to_string(), "Reject".to_string()],
                allow_always: true,
                form: None,
            };
            let json = serde_json::to_value(&reason).unwrap();
            assert_eq!(json["kind"], serde_json::to_value(kind).unwrap());
            assert_eq!(json["request_id"], "perm_1");
            assert_eq!(json["allow_always"], true);
            let back: AwaitingInputReason = serde_json::from_value(json).unwrap();
            assert_eq!(back, reason);
        }

        // Legacy payloads without the new fields decode with defaults, and the
        // added fields are omitted from the wire when empty/false.
        let legacy: AwaitingInputReason =
            serde_json::from_str(r#"{"kind":"permission","message":"Allow?"}"#).unwrap();
        assert!(legacy.request_id.is_none());
        assert!(legacy.options.is_empty());
        assert!(!legacy.allow_always);
        assert!(legacy.form.is_none());
        let json = serde_json::to_value(&legacy).unwrap();
        assert!(json.get("request_id").is_none());
        assert!(json.get("options").is_none());
        assert!(json.get("allow_always").is_none());
        assert!(json.get("form").is_none());

        let with_form = AwaitingInputReason {
            kind: AwaitingInputKind::Elicitation,
            message: "Fill in".to_string(),
            request_id: Some("acp-elic-1".to_string()),
            options: Vec::new(),
            allow_always: false,
            form: Some(crate::elicitation::ElicitationForm {
                title: Some("Details".to_string()),
                description: None,
                fields: Vec::new(),
            }),
        };
        let json = serde_json::to_value(&with_form).unwrap();
        let back: AwaitingInputReason = serde_json::from_value(json).unwrap();
        assert_eq!(back, with_form);
    }

    #[test]
    fn agent_session_info_activity_and_usage_default_and_round_trip() {
        // Legacy payloads (no activity/usage) decode unchanged.
        let legacy: AgentSessionInfo = serde_json::from_str(
            r#"{"id":"s1","agent":"opencode","task_id":null,"running":true,"headless":true}"#,
        )
        .unwrap();
        assert!(legacy.activity.is_none());
        assert!(legacy.usage.is_none());
        // Legacy payloads predate restore state; they are ready, not loading.
        assert_eq!(legacy.load_state, AgentLoadState::Ready);
        assert!(legacy.load_error.is_none());

        let live = AgentSessionInfo {
            activity: Some(AgentActivity::Responding),
            usage: Some(AgentUsage {
                input_tokens: 12,
                cost_usd: Some(0.02),
                ..Default::default()
            }),
            config_options: Vec::new(),
            modes: None,
            ..legacy
        };
        let json = serde_json::to_value(&live).unwrap();
        assert_eq!(json["activity"]["kind"], "responding");
        assert_eq!(json["usage"]["input_tokens"], 12);
        assert_eq!(json["load_state"], "ready");
        assert!(json.get("load_error").is_none());
        let back: AgentSessionInfo = serde_json::from_value(json).unwrap();
        assert_eq!(back.activity, live.activity);
        assert_eq!(back.usage, live.usage);
        assert_eq!(back.load_state, AgentLoadState::Ready);

        // A session with neither field omits both from the wire.
        let bare: AgentSessionInfo = serde_json::from_str(
            r#"{"id":"s2","agent":"opencode","task_id":null,"running":false,"headless":false}"#,
        )
        .unwrap();
        let json = serde_json::to_value(&bare).unwrap();
        assert!(json.get("activity").is_none());
        assert!(json.get("usage").is_none());
    }

    #[test]
    fn agent_session_info_load_state_round_trips_and_defaults_ready() {
        for state in [
            AgentLoadState::Ready,
            AgentLoadState::Loading,
            AgentLoadState::Failed,
        ] {
            let info = AgentSessionInfo {
                id: "s1".to_string(),
                agent: "opencode".to_string(),
                backend: AgentBackend::Acp,
                task_id: None,
                running: true,
                headless: false,
                session_id: Some("ses_acp".to_string()),
                awaiting_input: None,
                activity: None,
                usage: None,
                load_state: state,
                load_error: (state == AgentLoadState::Failed).then(|| "boom".to_string()),
                config_options: Vec::new(),
                modes: None,
                auth: None,
            };
            let json = serde_json::to_value(&info).unwrap();
            assert_eq!(json["load_state"], serde_json::json!(state));
            let back: AgentSessionInfo = serde_json::from_value(json).unwrap();
            assert_eq!(back.load_state, state);
            assert_eq!(back.load_error, info.load_error);
        }

        // A newer daemon's failed snapshot keeps the reason; successful ones
        // omit the empty error field.
        let failed = AgentSessionInfo {
            id: "s2".to_string(),
            agent: "opencode".to_string(),
            backend: AgentBackend::Acp,
            task_id: None,
            running: false,
            headless: false,
            session_id: None,
            awaiting_input: None,
            activity: None,
            usage: None,
            load_state: AgentLoadState::Failed,
            load_error: Some("cannot load session".to_string()),
            config_options: Vec::new(),
            modes: None,
            auth: None,
        };
        let json = serde_json::to_value(&failed).unwrap();
        assert_eq!(json["load_state"], "failed");
        assert_eq!(json["load_error"], "cannot load session");

        let ready = AgentSessionInfo {
            load_state: AgentLoadState::Ready,
            load_error: None,
            config_options: Vec::new(),
            modes: None,
            ..failed
        };
        let json = serde_json::to_value(&ready).unwrap();
        assert!(json.get("load_error").is_none());
    }

    #[test]
    fn provider_sessions_round_trip_and_keep_the_agent_scope() {
        let result = ProviderSessionsResult {
            agent: "opencode".to_string(),
            supported: true,
            can_resume: true,
            can_delete: false,
            sessions: vec![ProviderSessionEntry {
                agent: "opencode".to_string(),
                session_id: "provider-ses-1".to_string(),
                cwd: PathBuf::from("/work"),
                title: Some("Provider session".to_string()),
                updated_at: Some("2026-11-21T10:00:00Z".to_string()),
                local_session_id: Some("local-1".to_string()),
            }],
            next_cursor: Some("cursor-2".to_string()),
            error: None,
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["agent"], "opencode");
        assert_eq!(json["supported"], true);
        assert_eq!(json["can_resume"], true);
        assert_eq!(json["can_delete"], false);
        assert_eq!(json["sessions"][0]["session_id"], "provider-ses-1");
        assert_eq!(json["sessions"][0]["agent"], "opencode");
        assert_eq!(json["next_cursor"], "cursor-2");
        let back: ProviderSessionsResult = serde_json::from_value(json).unwrap();
        assert_eq!(back, result);

        // Capability and paging fields are additive for older producers.
        let lean: ProviderSessionsResult =
            serde_json::from_str(r#"{"agent":"pi","sessions":[]}"#).unwrap();
        assert!(!lean.supported);
        assert!(!lean.can_resume);
        assert!(!lean.can_delete);
        assert!(lean.next_cursor.is_none());
        assert!(lean.error.is_none());
    }

    #[test]
    fn agent_session_info_awaiting_input_defaults_and_round_trips() {
        // Legacy payloads without the key decode to `None`.
        let legacy: AgentSessionInfo = serde_json::from_str(
            r#"{"id":"s1","agent":"opencode","task_id":null,"running":true,"headless":true}"#,
        )
        .unwrap();
        assert!(legacy.awaiting_input.is_none());

        let with_reason = AgentSessionInfo {
            awaiting_input: Some(AwaitingInputReason {
                kind: AwaitingInputKind::Pinentry,
                message: "Enter passphrase:".to_string(),
                request_id: None,
                options: Vec::new(),
                allow_always: false,
                form: None,
            }),
            config_options: Vec::new(),
            modes: None,
            ..legacy
        };
        let json = serde_json::to_value(&with_reason).unwrap();
        assert_eq!(json["awaiting_input"]["kind"], "pinentry");
        let back: AgentSessionInfo = serde_json::from_value(json).unwrap();
        assert_eq!(back.awaiting_input, with_reason.awaiting_input);
    }

    #[test]
    fn agent_auth_state_round_trips_and_omits_empty_fields() {
        let state = AgentAuthState {
            status: AgentAuthStatus::Required,
            methods: vec![
                AgentAuthMethod {
                    id: "agent-login".to_string(),
                    name: "Sign in".to_string(),
                    description: Some("Opens the provider's device flow".to_string()),
                    kind: AgentAuthMethodKind::Agent,
                    supported: true,
                },
                AgentAuthMethod {
                    id: "terminal-login".to_string(),
                    name: "Terminal sign-in".to_string(),
                    description: None,
                    kind: AgentAuthMethodKind::Terminal,
                    supported: false,
                },
            ],
            logout_supported: true,
            method_id: None,
            message: Some("the agent requires sign-in".to_string()),
        };
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(json["status"], "required");
        assert_eq!(json["methods"][0]["kind"], "agent");
        assert_eq!(json["methods"][0]["supported"], true);
        assert_eq!(json["methods"][1]["kind"], "terminal");
        assert_eq!(json["methods"][1]["supported"], false);
        assert_eq!(json["logout_supported"], true);
        let back: AgentAuthState = serde_json::from_value(json).unwrap();
        assert_eq!(back, state);

        // No credentials are part of this shape. Only method metadata, status
        // and a safe message serialize.
        let empty = serde_json::to_value(AgentAuthState::default()).unwrap();
        assert_eq!(empty, serde_json::json!({ "status": "not_required" }));
        assert!(empty.get("methods").is_none());
        assert!(empty.get("message").is_none());
    }

    #[test]
    fn event_kind_from_name_accepts_pascal_case() {
        assert_eq!(
            EventKind::from_name("TaskAwaitingInput"),
            Some(EventKind::TaskAwaitingInput)
        );
        assert_eq!(
            EventKind::from_name("task_awaiting_input"),
            Some(EventKind::TaskAwaitingInput)
        );
    }

    #[test]
    fn task_into_notification_serializes_the_task() {
        let id = Uuid::new_v4();
        let task = Task {
            id,
            name: "t".to_string(),
            status: TaskStatus::Pending,
            attempt: 0,
            input: serde_json::json!({}),
            output: None,
            dedupe_key: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            error: None,
            failure: None,
            session_id: None,
            session_title: None,
            parent_id: None,
            root_id: None,
            interactive: false,
            agent: None,
            provider: None,
            model: None,
        };
        let n = task.into_notification().expect("task serialization");
        assert_eq!(n.method, push::TASK_UPDATED);
        assert_eq!(n.params["id"], id.to_string());
        assert_eq!(n.params["status"], "pending");
    }

    #[test]
    fn event_into_notification_serializes_the_event() {
        let event = Event {
            id: 7,
            kind: EventKind::TaskUpdated,
            payload: serde_json::json!({ "k": "v" }),
            created_at: Utc::now(),
        };
        let n = event.into_notification().expect("event serialization");
        assert_eq!(n.method, push::EVENT);
        assert_eq!(n.params["id"], 7);
        assert_eq!(n.params["kind"], "task_updated");
    }

    #[test]
    fn repo_info_source_round_trips_and_defaults_to_manual() {
        let info = RepoInfo {
            name: "favetto".to_string(),
            path: "/code/favetto".to_string(),
            available: true,
            active: false,
            source: RepoSource::Scanned,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["source"], "scanned");
        let back: RepoInfo = serde_json::from_value(json).unwrap();
        assert_eq!(back.source, RepoSource::Scanned);

        // Older payloads without `source` decode as `manual` (never reclaimed by
        // a base-directory scan).
        let legacy: RepoInfo =
            serde_json::from_str(r#"{"name":"favetto","path":"/code/favetto"}"#).unwrap();
        assert_eq!(legacy.source, RepoSource::Manual);
    }
}
