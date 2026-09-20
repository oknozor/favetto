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
    PrCreated,
    PrMerged,
    TicketCreated,
    TicketUpdated,
    ActionRunCompleted,
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
            EventKind::PrCreated => "pr_created",
            EventKind::PrMerged => "pr_merged",
            EventKind::TicketCreated => "ticket_created",
            EventKind::TicketUpdated => "ticket_updated",
            EventKind::ActionRunCompleted => "action_run_completed",
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
            "pr_created" => EventKind::PrCreated,
            "pr_merged" => EventKind::PrMerged,
            "ticket_created" => EventKind::TicketCreated,
            "ticket_updated" => EventKind::TicketUpdated,
            "action_run_completed" => EventKind::ActionRunCompleted,
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
            "PRCreated" => EventKind::PrCreated,
            "PRMerged" => EventKind::PrMerged,
            "TicketCreated" => EventKind::TicketCreated,
            "TicketUpdated" => EventKind::TicketUpdated,
            "ActionRunCompleted" => EventKind::ActionRunCompleted,
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

/// A configured external agent plus its live-session state, as returned by
/// `agents.list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCatalogEntry {
    pub name: String,
    pub command: String,
    pub default: bool,
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
