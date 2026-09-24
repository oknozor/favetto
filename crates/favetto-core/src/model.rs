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

/// Why a task ended in failure, so a controller can distinguish a genuine
/// agent failure from an infrastructure fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    TaskFinished => "task_finished", "TaskFinished", "A task ended (success or failure); used by `needs` dependencies.";
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
    /// Can be seeded with a prompt in interactive mode (`prompt_args` and/or
    /// `submit_prompt`), so a user-started task can run in the real TUI.
    #[serde(default)]
    pub interactive_prompt: bool,
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
                interactive_prompt: true,
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
            failure: None,
            session_id: Some("ses_1".to_string()),
            session_title: Some("Fix the widget".to_string()),
            parent_id: None,
            root_id: None,
            interactive: false,
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
    fn task_interactive_round_trips_and_defaults_false() {
        let task = Task {
            id: Uuid::new_v4(),
            name: "t".to_string(),
            status: TaskStatus::Pending,
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

    #[test]
    fn task_into_notification_serializes_the_task() {
        let id = Uuid::new_v4();
        let task = Task {
            id,
            name: "t".to_string(),
            status: TaskStatus::Pending,
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
}
