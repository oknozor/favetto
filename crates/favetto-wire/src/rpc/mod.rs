//! JSON-RPC 2.0-style wire protocol, MessagePack-encoded.
//!
//! The same [`Frame`] type is carried over the Unix socket and the WebSocket
//! transport, which is what makes "local attach" just a special case of remote
//! attach. Clients send [`Request`]s and receive [`Response`]s; servers push
//! [`Notification`]s (events, task updates, log lines) unsolicited.

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod messages;

pub use messages::*;

/// Correlation id for a request/response pair. The TUI client increments a counter.
pub type RequestId = u64;

/// Standard JSON-RPC error codes, plus favetto-specific ones.
pub mod error_code {
    pub const PARSE: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL: i32 = -32603;
    /// Caller not authenticated / token rejected.
    pub const UNAUTHORIZED: i32 = -32001;
}

/// A typed domain/RPC error, one variant per [`error_code`] constant.
///
/// The daemon's dispatch and parameter validation return this instead of bare
/// `(i32, String)` tuples. [`RpcError::to_object`] is the single place it is
/// converted to the wire [`RpcErrorObject`], so the numeric code and message
/// always come from the enum.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RpcError {
    /// Invalid JSON / MessagePack frame.
    #[error("{0}")]
    Parse(String),
    /// Not a valid request object.
    #[error("{0}")]
    InvalidRequest(String),
    /// Unknown method.
    #[error("{0}")]
    MethodNotFound(String),
    /// Invalid method parameters.
    #[error("{0}")]
    InvalidParams(String),
    /// Internal server error.
    #[error("{0}")]
    Internal(String),
    /// Missing or rejected bearer token.
    #[error("{0}")]
    Unauthorized(String),
}

impl RpcError {
    /// The numeric code sent on the wire for this error.
    pub fn code(&self) -> i32 {
        match self {
            Self::Parse(_) => error_code::PARSE,
            Self::InvalidRequest(_) => error_code::INVALID_REQUEST,
            Self::MethodNotFound(_) => error_code::METHOD_NOT_FOUND,
            Self::InvalidParams(_) => error_code::INVALID_PARAMS,
            Self::Internal(_) => error_code::INTERNAL,
            Self::Unauthorized(_) => error_code::UNAUTHORIZED,
        }
    }

    /// The human-readable message sent on the wire for this error.
    pub fn message(&self) -> &str {
        match self {
            Self::Parse(message)
            | Self::InvalidRequest(message)
            | Self::MethodNotFound(message)
            | Self::InvalidParams(message)
            | Self::Internal(message)
            | Self::Unauthorized(message) => message,
        }
    }

    /// Convert to the wire representation. This is the single conversion point
    /// between the typed domain error and the protocol.
    pub fn to_object(&self) -> RpcErrorObject {
        RpcErrorObject {
            code: self.code(),
            message: self.message().to_string(),
            data: None,
        }
    }
}

impl From<RpcError> for RpcErrorObject {
    fn from(error: RpcError) -> Self {
        error.to_object()
    }
}

/// Well-known client → server method names.
pub mod method {
    pub const PING: &str = "system.ping";
    /// Validate the daemon's on-disk config file.
    pub const CONFIG_VALIDATE: &str = "config.validate";
    pub const TASKS_LIST: &str = "tasks.list";
    /// Fetch a single task by id, including its stored output blob.
    pub const TASKS_GET: &str = "tasks.get";
    /// List a task's execution attempts (run history), oldest-first.
    pub const TASKS_RUNS: &str = "tasks.runs";
    /// Backfill a task's normalized agent-event feed from the live session's
    /// bounded ring buffer (non-durable; `seq` is the per-session cursor).
    pub const TASKS_EVENTS: &str = "tasks.events";
    /// Resolve the Favetto-created agent session associated with a task, by
    /// Favetto-local id. Used to restore a task's conversation on demand without
    /// enumerating global history.
    pub const TASKS_SESSION: &str = "tasks.session";
    pub const TASKS_CANCEL: &str = "tasks.cancel";
    /// Retry a terminal task, preserving its prior run history.
    pub const TASKS_RETRY: &str = "tasks.retry";
    /// Permanently delete a terminal task and its recorded run history.
    pub const TASKS_DELETE: &str = "tasks.delete";
    /// Start a catalog task by name.
    pub const TASKS_START: &str = "tasks.start";
    /// Start a one-shot task from an inline definition (not added to the catalog)
    /// and open its interactive agent session.
    pub const TASKS_START_ONESHOT: &str = "tasks.start_oneshot";
    /// List the task catalog.
    pub const CATALOG_LIST: &str = "catalog.list";
    /// Fetch a catalog task's raw Markdown source (for the preview pane).
    pub const CATALOG_GET: &str = "catalog.get";
    /// Add a task definition to the catalog (does not run it).
    pub const CATALOG_ADD: &str = "catalog.add";
    /// Replace an existing catalog task's raw Markdown source (does not run it).
    pub const CATALOG_UPDATE: &str = "catalog.update";
    /// Fetch the catalog workflow graph as Graphviz DOT (`dot` + persisted `path`).
    pub const WORKFLOW_GET: &str = "workflow.get";
    /// Fetch the runtime workflow graph for a root: task instances plus
    /// `ready`/`running`/`failed`/`blocked` buckets. No output blobs.
    pub const WORKFLOW_INSPECT: &str = "workflow.inspect";
    /// Create a runtime DAG of catalog tasks with per-instance dependencies.
    pub const WORKFLOW_CREATE: &str = "workflow.create";
    /// Add one runtime task to an existing workflow root with per-instance
    /// dependencies.
    pub const WORKFLOW_SPAWN: &str = "workflow.spawn";
    /// Cancel every non-terminal task in a workflow root, emitting
    /// `TaskCancelled` per task.
    pub const WORKFLOW_CANCEL: &str = "workflow.cancel";
    /// Retry a terminal task, preserving its prior run history. Single-task
    /// alias of `tasks.retry`.
    pub const WORKFLOW_RETRY: &str = "workflow.retry";
    pub const EVENTS_TAIL: &str = "events.tail";
    /// (Re)subscribe to the live event stream. Accepts `last_event_id` to replay
    /// missed events before switching to live delivery.
    pub const EVENTS_SUBSCRIBE: &str = "events.subscribe";
    /// List configured external agents and live agent sessions.
    pub const AGENTS_LIST: &str = "agents.list";
    /// List the durable ACP sessions Favetto created, by Favetto-local id.
    ///
    /// Additive: older daemons do not know it, and clients should fall back to
    /// `agents.list` when it fails. It never lists sessions created outside
    /// Favetto (ACP `session/list` is not implemented).
    pub const AGENTS_HISTORY: &str = "agents.history";
    /// Start an external agent session, or attach to a task's live session.
    pub const AGENTS_START: &str = "agents.start";
    /// Load a persisted ACP session by its Favetto-local id.
    ///
    /// The provider ACP id is never accepted here: the daemon resolves it from
    /// the durable record. A load never falls back to `session/new`, so an
    /// unsupported or failed restore is reported as an error.
    pub const AGENTS_LOAD: &str = "agents.load";
    /// Send a follow-up prompt to a structured (ACP) session.
    pub const AGENTS_PROMPT: &str = "agents.prompt";
    /// Interrupt the in-flight turn of a structured (ACP) session.
    pub const AGENTS_CANCEL: &str = "agents.cancel";
    /// Attach to a session: returns the session and a replay of its output.
    pub const AGENTS_ATTACH: &str = "agents.attach";
    /// Terminate a session.
    pub const AGENTS_CLOSE: &str = "agents.close";
    /// Answer a structured input request on a session's state channel.
    pub const AGENTS_REPLY: &str = "agents.reply";
    /// Answer an ACP v1 form elicitation request.
    ///
    /// Deliberately separate from [`AGENTS_REPLY`]: a form submission can never
    /// be mapped onto a permission option.
    pub const AGENTS_REPLY_ELICITATION: &str = "agents.reply_elicitation";
    /// Read a live session's advertised ACP config options and modes.
    pub const AGENTS_OPTIONS: &str = "agents.options";
    /// Change one config option advertised by a live ACP session.
    pub const AGENTS_SET_OPTION: &str = "agents.set_option";
    /// Switch a live ACP session to one of its advertised modes.
    pub const AGENTS_SET_MODE: &str = "agents.set_mode";
    /// List provider-owned ACP sessions through the agent's advertised
    /// `session/list`, distinct from Favetto's durable `agents.history` rows.
    pub const AGENTS_PROVIDER_SESSIONS: &str = "agents.provider_sessions";
    /// Resume a provider-owned ACP session by the provider's own session id,
    /// preferring `session/load` and falling back to `session/resume`.
    pub const AGENTS_PROVIDER_RESUME: &str = "agents.provider_resume";
    /// Delete a provider-owned ACP session through `session/delete`; Favetto's
    /// local metadata is left untouched.
    pub const AGENTS_PROVIDER_DELETE: &str = "agents.provider_delete";
    /// Complete an advertised agent-handled ACP v1 sign-in.
    ///
    /// Terminal methods are preserved in the session's auth state but rejected
    /// here: the daemon cannot safely run an interactive login process for a
    /// remote client.
    pub const AGENTS_AUTHENTICATE: &str = "agents.authenticate";
    /// Sign a session out through ACP `logout`, only when advertised.
    pub const AGENTS_LOGOUT: &str = "agents.logout";
    /// List configured providers and their available models.
    pub const PROVIDERS_LIST: &str = "providers.list";
    /// List cron schedules.
    pub const SCHEDULES_LIST: &str = "schedules.list";
    /// Create or update a cron schedule.
    pub const SCHEDULES_UPSERT: &str = "schedules.upsert";
    /// Delete a cron schedule.
    pub const SCHEDULES_DELETE: &str = "schedules.delete";
    /// List recent notifications.
    pub const NOTIFICATIONS_LIST: &str = "notifications.list";
    /// Send a test notification through a channel.
    pub const NOTIFICATIONS_TEST: &str = "notifications.test";
    /// Add a notification hook reacting to an event kind.
    pub const HOOKS_UPSERT: &str = "hooks.upsert";
    /// Aggregate persisted per-run token/cost usage into a per-bucket series for
    /// a day/week/month/year period, plus window totals.
    pub const USAGE_STATS: &str = "usage.stats";
    /// List known repositories and the active one.
    pub const REPOS_LIST: &str = "repos.list";
    /// Register a repository (by daemon-host path) as a known repo.
    pub const REPOS_ADD: &str = "repos.add";
    /// Remove a known repository by name.
    pub const REPOS_REMOVE: &str = "repos.remove";
    /// Set (or clear) the daemon-wide active repository.
    pub const REPOS_ACTIVATE: &str = "repos.activate";
    /// Re-scan the base directory and reconcile the repository registry.
    pub const REPOS_REFRESH: &str = "repos.refresh";
    /// List registered MCP server definitions with secrets redacted.
    pub const MCP_LIST: &str = "mcp.list";
    /// Fetch one registered MCP server definition.
    pub const MCP_GET: &str = "mcp.get";
    /// Register an MCP server (stdio command or HTTP/SSE URL).
    pub const MCP_ADD: &str = "mcp.add";
    /// Replace an existing MCP server definition.
    pub const MCP_UPDATE: &str = "mcp.update";
    /// Remove an MCP server definition.
    pub const MCP_REMOVE: &str = "mcp.remove";
    /// Enable or disable an MCP server definition.
    pub const MCP_SET_ENABLED: &str = "mcp.set_enabled";
    /// Approve (or revoke) launching a stdio MCP command.
    pub const MCP_APPROVE: &str = "mcp.approve";
}

/// Well-known server → client push (notification) method names.
pub mod push {
    pub const EVENT: &str = "event";
    pub const TASK_UPDATED: &str = "task.updated";
    /// A task and its recorded history were permanently deleted.
    pub const TASK_DELETED: &str = "task.deleted";
    /// The task catalog changed on disk; clients should re-fetch it.
    pub const CATALOG_UPDATED: &str = "catalog.updated";
    /// An agent session's child process exited.
    pub const AGENT_EXIT: &str = "agent.exit";
    /// A session's folded live state (activity/usage) changed.
    pub const AGENT_STATE: &str = "agent.state";
    /// One normalized agent-state event, in order. Non-durable: it carries no
    /// durable id and is backfilled via `tasks.events`, not the event log.
    pub const AGENT_EVENT: &str = "agent.event";
}

/// Every client → server method, paired with a one-line purpose. Consumed by the
/// generated remote-API reference (`docs/reference/remote-api.md`); keep in sync
/// with [`method`].
pub const CLIENT_METHODS: &[(&str, &str)] = &[
    (method::PING, "Liveness check."),
    (
        method::CONFIG_VALIDATE,
        "Validate the daemon's on-disk config file.",
    ),
    (
        method::TASKS_LIST,
        "List tasks (metadata only; output is omitted).",
    ),
    (
        method::TASKS_GET,
        "Fetch a single task by id, including its stored output.",
    ),
    (
        method::TASKS_RUNS,
        "List a task's execution attempts (run history).",
    ),
    (
        method::TASKS_EVENTS,
        "Backfill a task's normalized agent-event feed from its session's bounded \
         ring buffer (non-durable; `after`/`next_cursor` are per-session `seq`s).",
    ),
    (
        method::TASKS_SESSION,
        "Resolve the Favetto-created agent session associated with a task, by \
         Favetto-local id, preferring a live record over the most recently \
         updated ended one. Returns `null` when the task has no session.",
    ),
    (method::TASKS_CANCEL, "Cancel a task."),
    (
        method::TASKS_RETRY,
        "Retry a terminal task, preserving its prior run history.",
    ),
    (
        method::TASKS_DELETE,
        "Permanently delete a terminal task, its recorded runs and its \
         dependency edges. Non-terminal tasks are rejected.",
    ),
    (method::TASKS_START, "Start a catalog task by name."),
    (
        method::TASKS_START_ONESHOT,
        "Start a one-shot task from an inline definition (not added to the catalog) \
         and open its interactive agent session.",
    ),
    (method::CATALOG_LIST, "List the task catalog."),
    (
        method::CATALOG_GET,
        "Fetch a catalog task's raw Markdown source (for the preview pane).",
    ),
    (
        method::CATALOG_ADD,
        "Add a task definition to the catalog (does not run it).",
    ),
    (
        method::CATALOG_UPDATE,
        "Replace an existing catalog task's raw Markdown source (does not run it).",
    ),
    (
        method::WORKFLOW_GET,
        "Fetch the catalog workflow graph as Graphviz DOT plus a structured graph.",
    ),
    (
        method::WORKFLOW_INSPECT,
        "Fetch the runtime workflow graph for a root (task instances plus \
         ready/running/failed/blocked buckets).",
    ),
    (
        method::WORKFLOW_CREATE,
        "Create a runtime DAG of catalog tasks with per-instance dependencies. \
         Idempotent on `idempotency_key`.",
    ),
    (
        method::WORKFLOW_SPAWN,
        "Add one runtime task to an existing workflow root, optionally depending \
         on existing task ids.",
    ),
    (
        method::WORKFLOW_CANCEL,
        "Cancel every non-terminal task in a workflow root, emitting \
         `TaskCancelled` per task.",
    ),
    (
        method::WORKFLOW_RETRY,
        "Retry a terminal task, preserving its prior run history. Single-task \
         alias of `tasks.retry`.",
    ),
    (method::EVENTS_TAIL, "Tail persisted events."),
    (
        method::EVENTS_SUBSCRIBE,
        "(Re)subscribe to the live event stream; accepts `last_event_id` to replay \
         missed events before switching to live delivery.",
    ),
    (
        method::AGENTS_LIST,
        "List configured external agents and live agent sessions, including each \
         agent's `available` flag and capability flags.",
    ),
    (
        method::AGENTS_HISTORY,
        "List the durable ACP sessions Favetto created, by Favetto-local id, with \
         title, working directory, timestamps, lifecycle and loadability. Never \
         lists sessions created outside Favetto.",
    ),
    (
        method::AGENTS_START,
        "Start an external agent session, or attach to the task's existing live \
         session. `new` forces a fresh session.",
    ),
    (
        method::AGENTS_LOAD,
        "Load a persisted ACP session by its Favetto-local id (`session_id`). The \
         provider's ACP id is resolved from the durable record; an unsupported or \
         failed restore is an error and never starts a fresh session.",
    ),
    (
        method::AGENTS_PROMPT,
        "Send a follow-up prompt to a structured (ACP) agent session.",
    ),
    (
        method::AGENTS_CANCEL,
        "Interrupt the in-flight turn of a structured (ACP) agent session.",
    ),
    (
        method::AGENTS_ATTACH,
        "Attach to a session: returns the session and a replay of its output.",
    ),
    (method::AGENTS_CLOSE, "Terminate a session."),
    (
        method::AGENTS_REPLY,
        "Answer a structured input request on a session's state channel.",
    ),
    (
        method::AGENTS_REPLY_ELICITATION,
        "Answer an ACP v1 form elicitation request with accept, decline or cancel.",
    ),
    (
        method::AGENTS_OPTIONS,
        "Read a live ACP session's advertised config options and session modes. \
         The returned snapshot is the same one `agents.list` exposes.",
    ),
    (
        method::AGENTS_SET_OPTION,
        "Change one config option advertised by a live ACP session. The daemon \
         validates the option id and value against that session's current snapshot \
         and rejects values the agent did not offer.",
    ),
    (
        method::AGENTS_SET_MODE,
        "Switch a live ACP session to one of its advertised modes. Unknown mode ids \
         are rejected; the result carries the reconciled mode/option snapshot.",
    ),
    (
        method::AGENTS_PROVIDER_SESSIONS,
        "List provider-owned ACP sessions through the agent's advertised \
         `session/list`, with cwd filtering and opaque cursor pagination. \
         Distinct from `agents.history`: rows may never have been created by \
         Favetto. Unsupported agents return `supported: false` and a reason.",
    ),
    (
        method::AGENTS_PROVIDER_RESUME,
        "Resume a provider-owned ACP session by its provider ACP id. Prefers \
         `session/load` (with replay) when advertised, then `session/resume`; \
         imports the session into a Favetto-local record and returns that local \
         id for all subsequent calls.",
    ),
    (
        method::AGENTS_PROVIDER_DELETE,
        "Delete a provider-owned ACP session through the advertised \
         `session/delete`. Only the provider transcript is deleted; Favetto-local \
         metadata is untouched.",
    ),
    (
        method::AGENTS_AUTHENTICATE,
        "Complete an agent-handled ACP v1 sign-in. Only methods advertised by \
         the agent are accepted; terminal methods are rejected because the \
         daemon cannot run an interactive login process for a remote client.",
    ),
    (
        method::AGENTS_LOGOUT,
        "Sign a session out through ACP `logout` when the agent advertised it.",
    ),
    (
        method::PROVIDERS_LIST,
        "List configured providers and their available models.",
    ),
    (
        method::SCHEDULES_LIST,
        "List cron schedules, with the next occurrence (`next_run`, UTC) computed for \
         enabled entries.",
    ),
    (
        method::SCHEDULES_UPSERT,
        "Create or update a cron schedule.",
    ),
    (method::SCHEDULES_DELETE, "Delete a cron schedule."),
    (method::NOTIFICATIONS_LIST, "List recent notifications."),
    (
        method::NOTIFICATIONS_TEST,
        "Send a test notification through a channel.",
    ),
    (
        method::HOOKS_UPSERT,
        "Add a notification hook reacting to an event kind.",
    ),
    (
        method::USAGE_STATS,
        "Aggregate persisted per-run token/cost usage into a per-bucket series \
         (day/week/month/year) plus window totals.",
    ),
    (
        method::REPOS_LIST,
        "List known repositories and the daemon-wide active one.",
    ),
    (
        method::REPOS_ADD,
        "Register a repository (by daemon-host path) as a known repo.",
    ),
    (method::REPOS_REMOVE, "Remove a known repository by name."),
    (
        method::REPOS_ACTIVATE,
        "Set (or clear) the daemon-wide active repository.",
    ),
    (
        method::REPOS_REFRESH,
        "Re-scan the base directory and reconcile the repository registry.",
    ),
    (
        method::MCP_LIST,
        "List registered MCP server definitions. Secret values are never \
         returned; each entry reports only its source and `has_value`.",
    ),
    (
        method::MCP_GET,
        "Fetch one registered MCP server definition by name (secrets redacted).",
    ),
    (
        method::MCP_ADD,
        "Register an already-installed MCP server by stdio command or HTTP/SSE \
         URL. `approve` records stdio launch consent.",
    ),
    (
        method::MCP_UPDATE,
        "Replace a registered MCP server definition. A changed command/args \
         fingerprint revokes prior approval unless `approve` is set.",
    ),
    (
        method::MCP_REMOVE,
        "Remove a registered MCP server by name.",
    ),
    (
        method::MCP_SET_ENABLED,
        "Enable or disable a registered MCP server. Disabling never removes it.",
    ),
    (
        method::MCP_APPROVE,
        "Approve (or revoke) launching a stdio MCP command at the current \
         definition fingerprint.",
    ),
];

/// Every server → client push (notification), paired with a one-line purpose.
/// Consumed by the generated remote-API reference; keep in sync with [`push`].
pub const SERVER_PUSHES: &[(&str, &str)] = &[
    (push::EVENT, "A persisted event."),
    (push::TASK_UPDATED, "A task row changed."),
    (
        push::TASK_DELETED,
        "A task and its recorded run history were permanently deleted.",
    ),
    (
        push::CATALOG_UPDATED,
        "The task catalog changed on disk; clients should re-fetch it.",
    ),
    (push::AGENT_EXIT, "An agent session's child process exited."),
    (
        push::AGENT_STATE,
        "A session's folded live state (activity/usage) changed.",
    ),
    (
        push::AGENT_EVENT,
        "One normalized agent-state event, in order. Non-durable; backfill with \
         `tasks.events`.",
    ),
];

/// A client → server request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: RequestId,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

/// A structured error carried by a [`Response`]: the wire representation of an
/// [`RpcError`], built by [`RpcError::to_object`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcErrorObject {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// A server → client reply to a [`Request`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: RequestId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcErrorObject>,
}

impl Response {
    pub fn ok(id: RequestId, result: serde_json::Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Build an error reply from a typed [`RpcError`].
    pub fn error(id: RequestId, error: RpcError) -> Self {
        Self {
            id,
            result: None,
            error: Some(error.to_object()),
        }
    }
}

/// A unidirectional message. Clients send these rarely; servers use them for pushes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub method: String,
    pub params: serde_json::Value,
}

/// Top-level tagged envelope: exactly what a wire frame carries.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    Request(Request),
    Response(Response),
    Notification(Notification),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn assert_unique(table: &[(&str, &str)], label: &str) {
        let mut seen = HashSet::new();
        for (name, purpose) in table {
            assert!(!name.is_empty(), "{label}: empty name");
            assert!(
                !purpose.trim().is_empty(),
                "{label}: `{name}` has no purpose"
            );
            assert!(seen.insert(*name), "{label}: duplicate name `{name}`");
        }
    }

    #[test]
    fn client_methods_are_unique() {
        assert_unique(CLIENT_METHODS, "CLIENT_METHODS");
    }

    #[test]
    fn server_pushes_are_unique() {
        assert_unique(SERVER_PUSHES, "SERVER_PUSHES");
    }

    /// Guards against a typo in the doc tables: every documented name must be one
    /// of the module constants.
    #[test]
    fn doc_tables_match_constants() {
        let methods: HashSet<&str> = [
            method::PING,
            method::CONFIG_VALIDATE,
            method::TASKS_LIST,
            method::TASKS_GET,
            method::TASKS_RUNS,
            method::TASKS_EVENTS,
            method::TASKS_SESSION,
            method::TASKS_CANCEL,
            method::TASKS_RETRY,
            method::TASKS_DELETE,
            method::TASKS_START,
            method::TASKS_START_ONESHOT,
            method::CATALOG_LIST,
            method::CATALOG_GET,
            method::CATALOG_ADD,
            method::CATALOG_UPDATE,
            method::WORKFLOW_GET,
            method::WORKFLOW_INSPECT,
            method::WORKFLOW_CREATE,
            method::WORKFLOW_SPAWN,
            method::WORKFLOW_CANCEL,
            method::WORKFLOW_RETRY,
            method::EVENTS_TAIL,
            method::EVENTS_SUBSCRIBE,
            method::AGENTS_LIST,
            method::AGENTS_HISTORY,
            method::AGENTS_START,
            method::AGENTS_LOAD,
            method::AGENTS_PROMPT,
            method::AGENTS_CANCEL,
            method::AGENTS_ATTACH,
            method::AGENTS_CLOSE,
            method::AGENTS_REPLY,
            method::AGENTS_REPLY_ELICITATION,
            method::AGENTS_OPTIONS,
            method::AGENTS_SET_OPTION,
            method::AGENTS_SET_MODE,
            method::AGENTS_PROVIDER_SESSIONS,
            method::AGENTS_PROVIDER_RESUME,
            method::AGENTS_PROVIDER_DELETE,
            method::AGENTS_AUTHENTICATE,
            method::AGENTS_LOGOUT,
            method::PROVIDERS_LIST,
            method::SCHEDULES_LIST,
            method::SCHEDULES_UPSERT,
            method::SCHEDULES_DELETE,
            method::NOTIFICATIONS_LIST,
            method::NOTIFICATIONS_TEST,
            method::HOOKS_UPSERT,
            method::USAGE_STATS,
            method::REPOS_LIST,
            method::REPOS_ADD,
            method::REPOS_REMOVE,
            method::REPOS_ACTIVATE,
            method::REPOS_REFRESH,
            method::MCP_LIST,
            method::MCP_GET,
            method::MCP_ADD,
            method::MCP_UPDATE,
            method::MCP_REMOVE,
            method::MCP_SET_ENABLED,
            method::MCP_APPROVE,
        ]
        .into_iter()
        .collect();
        for (name, _) in CLIENT_METHODS {
            assert!(methods.contains(name), "undocumented constant `{name}`");
        }
        assert_eq!(methods.len(), CLIENT_METHODS.len());

        let pushes: HashSet<&str> = [
            push::EVENT,
            push::TASK_UPDATED,
            push::TASK_DELETED,
            push::CATALOG_UPDATED,
            push::AGENT_EXIT,
            push::AGENT_STATE,
            push::AGENT_EVENT,
        ]
        .into_iter()
        .collect();
        for (name, _) in SERVER_PUSHES {
            assert!(pushes.contains(name), "undocumented constant `{name}`");
        }
        assert_eq!(pushes.len(), SERVER_PUSHES.len());
    }

    /// Every typed error variant maps to its `error_code` constant and keeps its
    /// message verbatim when converted to the wire object.
    #[test]
    fn rpc_errors_map_to_wire_codes_and_messages() {
        let cases = [
            (RpcError::Parse("parse".to_string()), error_code::PARSE),
            (
                RpcError::InvalidRequest("invalid request".to_string()),
                error_code::INVALID_REQUEST,
            ),
            (
                RpcError::MethodNotFound("unknown method".to_string()),
                error_code::METHOD_NOT_FOUND,
            ),
            (
                RpcError::InvalidParams("invalid params".to_string()),
                error_code::INVALID_PARAMS,
            ),
            (RpcError::Internal("boom".to_string()), error_code::INTERNAL),
            (
                RpcError::Unauthorized("denied".to_string()),
                error_code::UNAUTHORIZED,
            ),
        ];

        for (error, code) in cases {
            assert_eq!(error.code(), code, "{error:?}");
            let object = error.to_object();
            assert_eq!(object.code, code, "{error:?}");
            assert_eq!(object.message, error.message(), "{error:?}");
            assert!(object.data.is_none(), "{error:?}");
            // `From` is the same conversion, so it must agree with `to_object`.
            assert_eq!(RpcErrorObject::from(error).message, object.message);
        }
    }

    /// A typed error becomes a wire reply with no result and the mapped code.
    #[test]
    fn response_error_carries_the_typed_error() {
        let response = Response::error(
            7,
            RpcError::MethodNotFound("unknown method: nope".to_string()),
        );
        assert_eq!(response.id, 7);
        assert!(response.result.is_none());
        let object = response.error.expect("error object");
        assert_eq!(object.code, error_code::METHOD_NOT_FOUND);
        assert_eq!(object.message, "unknown method: nope");
    }
}
