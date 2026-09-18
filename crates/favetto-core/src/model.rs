//! Domain model types shared across the daemon, TUI client, and future MCP servers.

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
/// API, then handed to the agent runtime which executes the referenced skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: Uuid,
    /// Name of the skill (folder under `skills/`) that defines how to run this task.
    pub skill: String,
    pub status: TaskStatus,
    /// Arbitrary input passed to the skill's `AGENT.md`/runtime.
    pub input: serde_json::Value,
    /// Produced by the skill on completion.
    pub output: Option<serde_json::Value>,
    /// Optional dedupe key. Inserting a second task with the same key is a no-op.
    pub dedupe_key: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    /// Human-readable failure reason when `status == Failed`.
    pub error: Option<String>,
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

/// A single turn in an agent conversation (the LLM chat).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub content: String,
}

impl ChatMessage {
    pub fn new(role: MessageRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
        }
    }
}

/// A chat session: the conversation the TUI displays and appends to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSession {
    pub id: String,
    /// Task this chat is attached to, if any.
    pub task_id: Option<String>,
    pub messages: Vec<ChatMessage>,
}
