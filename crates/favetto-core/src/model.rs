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
    /// Produced by the task on completion.
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
}

impl Task {
    /// Wrap this task as a server push so the daemon can broadcast it to TUI clients.
    pub fn into_notification(self) -> Notification {
        Notification {
            method: push::TASK_UPDATED.to_string(),
            params: serde_json::to_value(self).unwrap_or_default(),
        }
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
        let back: AgentCatalogEntry = serde_json::from_value(json).unwrap();
        assert_eq!(back.name, entry.name);
        assert_eq!(back.display_name, entry.display_name);
        assert_eq!(back.capabilities, entry.capabilities);
        assert!(back.default);

        // Old payloads without the new fields still decode, defaulting them.
        let legacy: AgentCatalogEntry =
            serde_json::from_str(r#"{"name":"pi","command":"pi","default":false,"sessions":[]}"#)
                .unwrap();
        assert_eq!(legacy.display_name, "");
        assert_eq!(legacy.capabilities, AgentCapabilities::default());
    }

    #[test]
    fn event_kind_round_trips() {
        let kinds = [
            EventKind::TaskCreated,
            EventKind::TaskUpdated,
            EventKind::TaskCompleted,
            EventKind::TaskFailed,
            EventKind::TaskCancelled,
            EventKind::TaskIdle,
            EventKind::TaskStarted,
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
        for kind in kinds {
            assert_eq!(
                EventKind::from_name(kind.as_str()),
                Some(kind.clone()),
                "from_name/as_str mismatch for {:?}",
                kind
            );
        }
    }
}
