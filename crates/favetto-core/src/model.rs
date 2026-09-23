//! Domain model types shared across the daemon and TUI client.

use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::rpc::{push, Notification};

/// Lifecycle of a single task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// A unit of work the favetto is driving.
///
/// Tasks are created by integrations, hooks, the scheduler, or manually via the
/// API, then handed to the executor which runs the referenced catalog task through
/// its external agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: Uuid,
    /// Name of the catalog task (the `*.md` file) that defines how to run this.
    pub name: String,
    pub status: TaskStatus,
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
}

impl Task {
    /// Wrap this task as a server push so the daemon can broadcast it to TUI clients.
    pub fn into_notification(self) -> Notification {
        Notification {
            method: push::TASK_UPDATED.to_string(),
            params: serde_json::to_value(self).unwrap_or_default(),
        }
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
/// Integration events (`TicketCreated`, `IssueCreated`, ...) arrive in M3/M4; M1
/// only emits task- and cron-flavoured synthetic events to exercise the pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    TaskCreated,
    TaskUpdated,
    TaskCompleted,
    TaskFailed,
    TaskCancelled,
    /// A task was enqueued and is idle (waiting to run).
    TaskIdle,
    /// A task began running.
    TaskStarted,
    /// A running task's agent is blocked waiting for user input.
    TaskAwaitingInput,
    /// A task ended (success or failure) — used by `needs` dependencies.
    TaskFinished,
    CronTick,
    // Integration events (M3+): emitted by webhook receivers and integrations.
    IssueCreated,
    IssueUpdated,
    IssueClosed,
    IssueReopened,
    IssueLabeled,
    IssueAssigned,
    IssueCommentCreated,
    PrCreated,
    PrMerged,
    PrClosed,
    PrReopened,
    PrSynchronized,
    PrReadyForReview,
    PrReviewRequested,
    PrUpdated,
    PrReviewSubmitted,
    PushReceived,
    TicketCreated,
    TicketUpdated,
    ActionRunCompleted,
    CheckSuiteCompleted,
    CheckRunCompleted,
    EmailReceived,
    /// Generic marker for synthetic events that drive the TUI before real
    /// integrations exist (M1 only).
    Synthetic,
}

impl EventKind {
    /// Stable string form used for persistence and on-the-wire encoding.
    pub fn as_str(&self) -> &'static str {
        match self {
            EventKind::TaskCreated => "task_created",
            EventKind::TaskUpdated => "task_updated",
            EventKind::TaskCompleted => "task_completed",
            EventKind::TaskFailed => "task_failed",
            EventKind::TaskCancelled => "task_cancelled",
            EventKind::TaskIdle => "task_idle",
            EventKind::TaskStarted => "task_started",
            EventKind::TaskAwaitingInput => "task_awaiting_input",
            EventKind::TaskFinished => "task_finished",
            EventKind::CronTick => "cron_tick",
            EventKind::IssueCreated => "issue_created",
            EventKind::IssueUpdated => "issue_updated",
            EventKind::IssueClosed => "issue_closed",
            EventKind::IssueReopened => "issue_reopened",
            EventKind::IssueLabeled => "issue_labeled",
            EventKind::IssueAssigned => "issue_assigned",
            EventKind::IssueCommentCreated => "issue_comment_created",
            EventKind::PrCreated => "pr_created",
            EventKind::PrMerged => "pr_merged",
            EventKind::PrClosed => "pr_closed",
            EventKind::PrReopened => "pr_reopened",
            EventKind::PrSynchronized => "pr_synchronized",
            EventKind::PrReadyForReview => "pr_ready_for_review",
            EventKind::PrReviewRequested => "pr_review_requested",
            EventKind::PrUpdated => "pr_updated",
            EventKind::PrReviewSubmitted => "pr_review_submitted",
            EventKind::PushReceived => "push_received",
            EventKind::TicketCreated => "ticket_created",
            EventKind::TicketUpdated => "ticket_updated",
            EventKind::ActionRunCompleted => "action_run_completed",
            EventKind::CheckSuiteCompleted => "check_suite_completed",
            EventKind::CheckRunCompleted => "check_run_completed",
            EventKind::EmailReceived => "email_received",
            EventKind::Synthetic => "synthetic",
        }
    }
}

impl EventKind {
    /// Every [`EventKind`], in declaration order. Kept in sync with the enum by
    /// the `all_covers_every_kind` test.
    pub const ALL: &'static [EventKind] = &[
        EventKind::TaskCreated,
        EventKind::TaskUpdated,
        EventKind::TaskCompleted,
        EventKind::TaskFailed,
        EventKind::TaskCancelled,
        EventKind::TaskIdle,
        EventKind::TaskStarted,
        EventKind::TaskAwaitingInput,
        EventKind::TaskFinished,
        EventKind::CronTick,
        EventKind::IssueCreated,
        EventKind::IssueUpdated,
        EventKind::IssueClosed,
        EventKind::IssueReopened,
        EventKind::IssueLabeled,
        EventKind::IssueAssigned,
        EventKind::IssueCommentCreated,
        EventKind::PrCreated,
        EventKind::PrMerged,
        EventKind::PrClosed,
        EventKind::PrReopened,
        EventKind::PrSynchronized,
        EventKind::PrReadyForReview,
        EventKind::PrReviewRequested,
        EventKind::PrUpdated,
        EventKind::PrReviewSubmitted,
        EventKind::PushReceived,
        EventKind::TicketCreated,
        EventKind::TicketUpdated,
        EventKind::ActionRunCompleted,
        EventKind::CheckSuiteCompleted,
        EventKind::CheckRunCompleted,
        EventKind::EmailReceived,
        EventKind::Synthetic,
    ];

    /// A one-line explanation of when this event is emitted, used by the
    /// generated event-kinds reference.
    pub fn description(&self) -> &'static str {
        match self {
            EventKind::TaskCreated => "A task row was created.",
            EventKind::TaskUpdated => "A task row changed.",
            EventKind::TaskCompleted => "A task completed successfully (terminal).",
            EventKind::TaskFailed => "A task failed (terminal).",
            EventKind::TaskCancelled => "A task was cancelled (terminal).",
            EventKind::TaskIdle => "A task was enqueued and is waiting to run.",
            EventKind::TaskStarted => "A task began running.",
            EventKind::TaskAwaitingInput => {
                "A running task's agent is blocked waiting for user input."
            }
            EventKind::TaskFinished => {
                "A task ended (success or failure); used by `needs` dependencies."
            }
            EventKind::CronTick => "A scheduled cron job fired.",
            EventKind::IssueCreated => "An issue was created.",
            EventKind::IssueUpdated => "An issue was edited.",
            EventKind::IssueClosed => "An issue was closed.",
            EventKind::IssueReopened => "An issue was reopened.",
            EventKind::IssueLabeled => "A label was added to or removed from an issue.",
            EventKind::IssueAssigned => "An issue was assigned.",
            EventKind::IssueCommentCreated => "A comment was posted on an issue.",
            EventKind::PrCreated => "A pull request was opened.",
            EventKind::PrMerged => "A pull request was merged.",
            EventKind::PrClosed => "A pull request was closed without merging.",
            EventKind::PrReopened => "A pull request was reopened.",
            EventKind::PrSynchronized => "A pull request received new commits.",
            EventKind::PrReadyForReview => "A draft pull request was marked ready for review.",
            EventKind::PrReviewRequested => "A review was requested on a pull request.",
            EventKind::PrUpdated => "A pull request was edited.",
            EventKind::PrReviewSubmitted => "A review was submitted on a pull request.",
            EventKind::PushReceived => "A push was received on a branch.",
            EventKind::TicketCreated => "A ticket was created (integration event).",
            EventKind::TicketUpdated => "A ticket was updated (integration event).",
            EventKind::ActionRunCompleted => "A workflow run completed.",
            EventKind::CheckSuiteCompleted => "A check suite completed.",
            EventKind::CheckRunCompleted => "A check run completed.",
            EventKind::EmailReceived => "An email was received (integration event).",
            EventKind::Synthetic => "A synthetic marker event used before real integrations exist.",
        }
    }
}

impl FromStr for EventKind {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_name(s).ok_or(())
    }
}

impl EventKind {
    /// Parse an event name in either the wire `snake_case` form or the spec's
    /// `PascalCase` form (e.g. `"ticket_created"` or `"TicketCreated"`).
    pub fn from_name(s: &str) -> Option<Self> {
        let kind = match s {
            "task_created" => EventKind::TaskCreated,
            "task_updated" => EventKind::TaskUpdated,
            "task_completed" => EventKind::TaskCompleted,
            "task_failed" => EventKind::TaskFailed,
            "task_cancelled" => EventKind::TaskCancelled,
            "task_idle" => EventKind::TaskIdle,
            "task_started" => EventKind::TaskStarted,
            "task_awaiting_input" => EventKind::TaskAwaitingInput,
            "task_finished" => EventKind::TaskFinished,
            "cron_tick" => EventKind::CronTick,
            "issue_created" => EventKind::IssueCreated,
            "issue_updated" => EventKind::IssueUpdated,
            "issue_closed" => EventKind::IssueClosed,
            "issue_reopened" => EventKind::IssueReopened,
            "issue_labeled" => EventKind::IssueLabeled,
            "issue_assigned" => EventKind::IssueAssigned,
            "issue_comment_created" => EventKind::IssueCommentCreated,
            "pr_created" => EventKind::PrCreated,
            "pr_merged" => EventKind::PrMerged,
            "pr_closed" => EventKind::PrClosed,
            "pr_reopened" => EventKind::PrReopened,
            "pr_synchronized" => EventKind::PrSynchronized,
            "pr_ready_for_review" => EventKind::PrReadyForReview,
            "pr_review_requested" => EventKind::PrReviewRequested,
            "pr_updated" => EventKind::PrUpdated,
            "pr_review_submitted" => EventKind::PrReviewSubmitted,
            "push_received" => EventKind::PushReceived,
            "ticket_created" => EventKind::TicketCreated,
            "ticket_updated" => EventKind::TicketUpdated,
            "action_run_completed" => EventKind::ActionRunCompleted,
            "check_suite_completed" => EventKind::CheckSuiteCompleted,
            "check_run_completed" => EventKind::CheckRunCompleted,
            "email_received" => EventKind::EmailReceived,
            "synthetic" => EventKind::Synthetic,
            "TaskCreated" => EventKind::TaskCreated,
            "TaskUpdated" => EventKind::TaskUpdated,
            "TaskCompleted" => EventKind::TaskCompleted,
            "TaskFailed" => EventKind::TaskFailed,
            "TaskCancelled" => EventKind::TaskCancelled,
            "TaskIdle" => EventKind::TaskIdle,
            "TaskStarted" => EventKind::TaskStarted,
            "TaskAwaitingInput" => EventKind::TaskAwaitingInput,
            "TaskFinished" => EventKind::TaskFinished,
            "CronTick" => EventKind::CronTick,
            "IssueCreated" => EventKind::IssueCreated,
            "IssueUpdated" => EventKind::IssueUpdated,
            "IssueClosed" => EventKind::IssueClosed,
            "IssueReopened" => EventKind::IssueReopened,
            "IssueLabeled" => EventKind::IssueLabeled,
            "IssueAssigned" => EventKind::IssueAssigned,
            "IssueCommentCreated" => EventKind::IssueCommentCreated,
            "PRCreated" => EventKind::PrCreated,
            "PRMerged" => EventKind::PrMerged,
            "PRClosed" => EventKind::PrClosed,
            "PRReopened" => EventKind::PrReopened,
            "PRSynchronized" => EventKind::PrSynchronized,
            "PRReadyForReview" => EventKind::PrReadyForReview,
            "PRReviewRequested" => EventKind::PrReviewRequested,
            "PRUpdated" => EventKind::PrUpdated,
            "PRReviewSubmitted" => EventKind::PrReviewSubmitted,
            "PushReceived" => EventKind::PushReceived,
            "TicketCreated" => EventKind::TicketCreated,
            "TicketUpdated" => EventKind::TicketUpdated,
            "ActionRunCompleted" => EventKind::ActionRunCompleted,
            "CheckSuiteCompleted" => EventKind::CheckSuiteCompleted,
            "CheckRunCompleted" => EventKind::CheckRunCompleted,
            "EmailReceived" => EventKind::EmailReceived,
            "Synthetic" => EventKind::Synthetic,
            _ => return None,
        };
        Some(kind)
    }
}

/// An immutable record of something that happened.
///
/// `id` is a monotonically increasing integer (SQLite `INTEGER PRIMARY KEY`
/// AUTOINCREMENT) and doubles as the resume cursor for remote TUI subscriptions:
/// a client reconnecting with `last_event_id` replays everything after it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    pub kind: EventKind,
    pub payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

impl Event {
    /// Wrap this event as a server push so the daemon can broadcast it to TUI clients.
    pub fn into_notification(self) -> Notification {
        Notification {
            method: push::EVENT.to_string(),
            params: serde_json::to_value(self).unwrap_or_default(),
        }
    }
}

/// Role of a [`ChatMessage`] in an agent conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    /// A prompt was detected but could not be classified further.
    Other,
}

/// Why a session is considered blocked on the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AwaitingInputReason {
    pub kind: AwaitingInputKind,
    /// The prompt text (the last visible lines), for context.
    pub message: String,
}

/// A live external-agent session (a PTY running an agent CLI on the daemon).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSessionInfo {
    pub id: String,
    /// Name of the `[agents.*]` entry that launched it.
    pub agent: String,
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
}

/// What an external agent CLI can do, so the daemon and TUI can adapt without
/// hard-coding a specific CLI's behaviour.
///
/// Every field defaults to `false` and is additive on the wire: an entry written
/// by an older version decodes with all capabilities off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCapabilities {
    /// Can run an attachable interactive TUI (the Agent panel).
    #[serde(default)]
    pub interactive: bool,
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
}

/// Default for a payload that omits `available`: older daemons/clients only know
/// about agents they can actually launch, so treat them as available.
fn default_available() -> bool {
    true
}

/// A configured external agent plus its live-session state, as returned by
/// `agents.list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
pub struct Schedule {
    pub id: String,
    pub cron: String,
    /// The catalog task name to enqueue on each fire.
    pub task: String,
    pub input: serde_json::Value,
    pub enabled: bool,
    pub last_run: Option<DateTime<Utc>>,
}

/// A persisted notification record (sent history).
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    fn agent_catalog_entry_round_trips_capabilities() {
        let entry = AgentCatalogEntry {
            name: "opencode".to_string(),
            display_name: "OpenCode".to_string(),
            command: "opencode".to_string(),
            default: true,
            available: true,
            capabilities: AgentCapabilities {
                interactive: true,
                headless: true,
                resume: true,
                model_selection: true,
                providers: true,
                structured_output: true,
                reports_session_id: true,
                prompt_prefill: true,
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
            input: serde_json::json!({}),
            output: None,
            dedupe_key: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            error: None,
            session_id: Some("ses_1".to_string()),
            session_title: Some("Fix the widget".to_string()),
            parent_id: None,
            root_id: None,
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
    fn task_lineage_round_trips() {
        let parent = Uuid::new_v4();
        let root = Uuid::new_v4();
        let task = Task {
            id: Uuid::new_v4(),
            name: "t".to_string(),
            status: TaskStatus::Succeeded,
            input: serde_json::json!({}),
            output: None,
            dedupe_key: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            error: None,
            session_id: None,
            session_title: None,
            parent_id: Some(parent),
            root_id: Some(root),
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
    fn event_kind_round_trips() {
        for kind in EventKind::ALL {
            assert_eq!(
                EventKind::from_name(kind.as_str()),
                Some(kind.clone()),
                "from_name/as_str mismatch for {:?}",
                kind
            );
        }
    }

    #[test]
    fn all_covers_every_kind() {
        // Every `ALL` entry survives a round-trip, and the list has no duplicates.
        let mut names: Vec<&str> = EventKind::ALL.iter().map(EventKind::as_str).collect();
        let len = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), len, "EventKind::ALL contains a duplicate");
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
    fn awaiting_input_reason_round_trips() {
        for kind in [
            AwaitingInputKind::Permission,
            AwaitingInputKind::Confirmation,
            AwaitingInputKind::Choice,
            AwaitingInputKind::Pinentry,
            AwaitingInputKind::Other,
        ] {
            let reason = AwaitingInputReason {
                kind,
                message: "Allow once / Allow always / Reject".to_string(),
            };
            let json = serde_json::to_value(&reason).unwrap();
            assert_eq!(json["kind"], serde_json::to_value(kind).unwrap());
            let back: AwaitingInputReason = serde_json::from_value(json).unwrap();
            assert_eq!(back, reason);
        }
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
            }),
            ..legacy
        };
        let json = serde_json::to_value(&with_reason).unwrap();
        assert_eq!(json["awaiting_input"]["kind"], "pinentry");
        let back: AgentSessionInfo = serde_json::from_value(json).unwrap();
        assert_eq!(back.awaiting_input, with_reason.awaiting_input);
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
}
