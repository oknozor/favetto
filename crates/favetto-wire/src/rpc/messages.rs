//! Typed request/response payloads for the remote API.
//!
//! The wire envelope ([`Request`](super::Request), [`Response`](super::Response),
//! [`Notification`](super::Notification)) deliberately stays `serde_json::Value`
//! based for backwards compatibility, but every client → server method has a
//! public, rustdoc'd `Params` and `Result` type here. The daemon deserializes
//! params into these types and serializes results from them, so the shapes live
//! in one place and stay in sync with the generated
//! `docs/reference/remote-api.md`.
//!
//! Each method is described by a marker type implementing [`RpcCall`]; the
//! [`CLIENT_CALLS`] / [`PUSH_CALLS`] registries pair a method with the JSON
//! Schema of its params and result so the reference is generated from the types
//! rather than hand-maintained.

use std::path::PathBuf;

// `JsonSchema` is only needed by the schema registries, gated behind the `schema` feature.
#[cfg(feature = "schema")]
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::agent_state::{
    AgentActivity, AgentCommand, AgentPlanEntry, AgentStateEvent, AgentUsage, InputReply,
    SessionConfigOption, SessionConfigValue, SessionModes, UsagePeriod, UsageStats,
};
use crate::elicitation::ElicitationReply;
use crate::model::{
    AgentAuthState, AgentCatalogEntry, AgentSessionHistoryEntry, AgentSessionInfo, Event,
    NotificationRecord, ProviderSessionsResult, RepoInfo, Schedule, Task, TaskRun,
};
use crate::task_var::TaskVar;
use crate::workflow::{WorkflowCancelResult, WorkflowCreateResult, WorkflowGraph, WorkflowInspect};

use super::method;

/// Deserialize an optional field leniently: a missing or ill-typed value becomes
/// `None`, matching the `params.get(..).and_then(..)` chains these typed params
/// replace. Required fields deliberately do not use this.
pub fn lenient<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: DeserializeOwned,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| serde_json::from_value(value).ok()))
}

/// Params for a method that takes none. Ignored by the daemon.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EmptyParams {}

// ---------------------------------------------------------------------------
// system
// ---------------------------------------------------------------------------

/// `system.ping` params: none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PingParams {}

/// `system.ping` result: daemon liveness plus its current working directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PingResult {
    /// Always `true` for a live daemon.
    pub pong: bool,
    /// The daemon process's current working directory.
    pub cwd: String,
}

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

/// `config.validate` params: none ([`EmptyParams`]).
///
/// `config.validate` result: whether the daemon's on-disk config file parsed,
/// the resolved path it was read from, and the parse/deserialization errors when
/// it did not. A missing file is valid (defaults apply).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ConfigValidateResult {
    /// True when the file parsed, or is absent so defaults apply.
    pub valid: bool,
    /// The resolved config path the daemon was started with.
    pub path: String,
    /// One message per parse/deserialization failure; empty when `valid`.
    pub errors: Vec<String>,
}

// ---------------------------------------------------------------------------
// tasks
// ---------------------------------------------------------------------------

/// `tasks.list` params. `limit` defaults to 500 and caps at 2000.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TasksListParams {
    #[serde(default, deserialize_with = "lenient")]
    pub limit: Option<u64>,
}

/// `tasks.list` result: task summaries, newest first. Output blobs are omitted;
/// fetch them with `tasks.get`.
pub type TasksListResult = Vec<Task>;

/// `tasks.get` / `tasks.cancel` / `tasks.retry` params.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TaskIdParams {
    pub id: Uuid,
}

/// `tasks.get` result: the full task, including its stored output blob.
pub type TasksGetResult = Task;

/// `tasks.runs` result: every execution attempt for the task, oldest-first.
pub type TasksRunsResult = Vec<TaskRun>;

/// `tasks.events` params: the task id plus a per-session `seq` cursor. `after`
/// is exclusive (return events with `seq > after`); `limit` defaults to 500 and
/// caps at 2000.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TasksEventsParams {
    pub id: Uuid,
    #[serde(default, deserialize_with = "lenient")]
    pub after: Option<u64>,
    #[serde(default, deserialize_with = "lenient")]
    pub limit: Option<u64>,
}

/// `tasks.events` result: the task's structured feed, oldest-first. `session_id`
/// is the resolved live (or most recent) session, `None` when the task has no
/// structured state source. This buffer is in-memory only; after a reload the
/// web renders the durable `RunSummary` persisted in `task.output` instead (and
/// falls back to the plain output view when there is none).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TasksEventsResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub events: Vec<AgentEventPush>,
    pub next_cursor: u64,
    /// The session's current plan at `next_cursor`, when it has one. `None`
    /// means there is no structured session; `Some(empty)` means the agent
    /// explicitly cleared the plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<Vec<AgentPlanEntry>>,
    /// The session's current command list at `next_cursor`, with the same
    /// `None`/empty distinction as [`Self::plan`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commands: Option<Vec<AgentCommand>>,
}

/// `tasks.cancel` result: the cancelled task, with `output` omitted.
pub type TasksCancelResult = Task;

/// `tasks.retry` result: the re-enqueued task, with `output` omitted.
pub type TasksRetryResult = Task;

/// `tasks.start` params. `input` defaults to JSON null; `interactive` defaults
/// to false (headless), so only the TUI opts into the live embedded TUI.
///
/// `agent`, `provider`, and `model` are optional per-launch overrides. They take
/// precedence over the catalog definition's front-matter and the daemon defaults
/// for this task, and are persisted on the task row so retries keep them.
/// `provider`/`model` are treated as one pair: overriding either never mixes an
/// overridden model with the definition's provider. Action tasks reject them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TasksStartParams {
    pub name: String,
    #[serde(default, deserialize_with = "lenient")]
    pub input: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "lenient")]
    pub interactive: Option<bool>,
    #[serde(default, deserialize_with = "lenient")]
    pub agent: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub provider: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub model: Option<String>,
}

/// `tasks.start` result: the enqueued task.
pub type TasksStartResult = Task;

/// `tasks.start_oneshot` params. `rows`/`cols` default to 24/80.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct StartOneshotParams {
    #[serde(default, deserialize_with = "lenient")]
    pub agent: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub provider: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub model: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub cwd: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub rows: Option<u64>,
    #[serde(default, deserialize_with = "lenient")]
    pub cols: Option<u64>,
}

/// `tasks.start_oneshot` result: the inline task, its live session, and the
/// current full-screen frame (base64) so the client starts in sync.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct StartOneshotResult {
    pub task: Task,
    pub session: AgentSessionInfo,
    /// Base64-encoded `vt100` screen frame.
    pub data: String,
    /// See [`AgentAttachResult::seq`].
    #[serde(default)]
    pub seq: u64,
}

// ---------------------------------------------------------------------------
// catalog
// ---------------------------------------------------------------------------

/// A catalog task as listed by `catalog.list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CatalogEntry {
    pub name: String,
    pub agent: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub schedule: Option<String>,
    /// How the task runs: `"agent"` (the default) or `"action"`. Defaulted for
    /// older daemons that predate `kind`.
    #[serde(default)]
    pub kind: String,
    pub needs: Option<String>,
    /// Manual input variables declared with `[[vars]]`.
    pub vars: Vec<TaskVar>,
    /// The raw prompt template.
    pub prompt: String,
    /// For a repo-local task, the repo namespace it belongs to (the `<name>` in
    /// `repo:<name>/<path>`); `None` for global tasks.
    #[serde(default)]
    pub repo: Option<String>,
}

/// `catalog.list` result: every catalog definition, sorted by name.
pub type CatalogListResult = Vec<CatalogEntry>;

/// `catalog.get` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CatalogGetParams {
    pub name: String,
}

/// `catalog.get` result: the task's raw Markdown source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CatalogGetResult {
    pub markdown: String,
}

/// `catalog.add` params. `spawn_new_root` defaults to false, `prompt` to "".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CatalogAddParams {
    pub name: String,
    #[serde(default, deserialize_with = "lenient")]
    pub agent: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub provider: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub model: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub cwd: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub schedule: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub needs: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub spawn: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub spawn_file: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub spawn_new_root: Option<bool>,
    #[serde(default, deserialize_with = "lenient")]
    pub prompt: Option<String>,
}

/// `catalog.add` result: the added definition. Note this deliberately omits
/// `prompt` and the `spawn*` fields, unlike [`CatalogEntry`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CatalogAddResult {
    pub name: String,
    pub agent: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub schedule: Option<String>,
    pub needs: Option<String>,
    pub vars: Vec<TaskVar>,
}

/// `catalog.update` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CatalogUpdateParams {
    pub name: String,
    pub markdown: String,
}

/// `catalog.update` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CatalogUpdateResult {
    pub updated: bool,
}

// ---------------------------------------------------------------------------
// workflow
// ---------------------------------------------------------------------------

/// `workflow.get` result: the catalog graph as Graphviz DOT, its persisted path,
/// and the structured graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkflowGetResult {
    pub dot: String,
    pub path: String,
    pub graph: WorkflowGraph,
}

/// `workflow.inspect` / `workflow.cancel` params.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkflowInspectParams {
    pub root_id: Uuid,
}

/// `workflow.inspect` result: the runtime graph for the root.
pub type WorkflowInspectResult = WorkflowInspect;

/// `workflow.retry` params. The workflow API names the task id `task_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkflowRetryParams {
    pub task_id: Uuid,
}

/// One node of a `workflow.create` request. `key` is local to the request and is
/// what sibling nodes reference from `depends_on`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkflowCreateTask {
    pub key: String,
    pub name: String,
    #[serde(default, deserialize_with = "lenient")]
    pub input: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "lenient")]
    pub depends_on: Option<Vec<String>>,
}

/// `workflow.create` params. `idempotency_key` scopes the per-node dedupe keys;
/// `root_id` optionally extends an existing root.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkflowCreateParams {
    pub idempotency_key: String,
    #[serde(default, deserialize_with = "lenient")]
    pub root_id: Option<Uuid>,
    #[serde(default, deserialize_with = "lenient")]
    pub tasks: Option<Vec<WorkflowCreateTask>>,
}

/// `workflow.create` result.
pub type WorkflowCreateResultBody = WorkflowCreateResult;

/// `workflow.spawn` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkflowSpawnParams {
    pub name: String,
    #[serde(default, deserialize_with = "lenient")]
    pub input: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "lenient")]
    pub root_id: Option<Uuid>,
    #[serde(default, deserialize_with = "lenient")]
    pub depends_on: Option<Vec<Uuid>>,
    #[serde(default, deserialize_with = "lenient")]
    pub dedupe_key: Option<String>,
}

/// `workflow.spawn` result: the spawned task.
pub type WorkflowSpawnResult = Task;

/// `workflow.cancel` result.
pub type WorkflowCancelResultBody = WorkflowCancelResult;

/// `workflow.retry` result: the re-enqueued task, with `output` omitted.
pub type WorkflowRetryResult = Task;

// ---------------------------------------------------------------------------
// events
// ---------------------------------------------------------------------------

/// `events.tail` params. `limit` defaults to 50 and caps at 1000.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EventsTailParams {
    #[serde(default, deserialize_with = "lenient")]
    pub limit: Option<u64>,
}

/// `events.tail` result: the most recent persisted events.
pub type EventsTailResult = Vec<Event>;

/// `events.subscribe` params. A malformed request degrades to "no replay".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EventsSubscribeParams {
    #[serde(default, deserialize_with = "lenient")]
    pub last_event_id: Option<i64>,
}

/// `events.subscribe` result: acknowledgement of the subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EventsSubscribeResult {
    pub subscribed: bool,
}

// ---------------------------------------------------------------------------
// agents
// ---------------------------------------------------------------------------

/// `agents.list` result: configured agents plus their live sessions.
pub type AgentsListResult = Vec<AgentCatalogEntry>;

/// `agents.history` params. `limit` defaults to 50 and caps at 500.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentsHistoryParams {
    #[serde(default, deserialize_with = "lenient")]
    pub limit: Option<u64>,
}

/// `agents.history` result: Favetto-created persisted sessions, most recently
/// updated first. Each row is keyed by the Favetto-local id; the provider's ACP
/// id is never exposed.
pub type AgentsHistoryResult = Vec<AgentSessionHistoryEntry>;

/// `agents.load` params: the Favetto-local id of a persisted session.
///
/// Despite the field name, this is **not** an agent/provider session id. The
/// daemon resolves the provider ACP id from its durable record and `session/load`
/// never falls back to creating a fresh session.
pub type AgentLoadParams = SessionIdParams;

/// `agents.start` params. `rows`/`cols` default to 24/80, `new` to false.
///
/// With `task_id` and without `new`, a live session for that task is returned
/// as-is (its feed is backfilled with `tasks.events`) rather than starting a
/// duplicate. `new` forces a fresh session.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct StartAgentParams {
    #[serde(default, deserialize_with = "lenient")]
    pub agent: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub task_id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub prompt: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub session_id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub rows: Option<u64>,
    #[serde(default, deserialize_with = "lenient")]
    pub cols: Option<u64>,
    #[serde(default, deserialize_with = "lenient")]
    pub new: Option<bool>,
    #[serde(default, deserialize_with = "lenient")]
    pub provider: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub model: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub cwd: Option<PathBuf>,
}

/// `agents.start` / `agents.attach` result: the session plus a replay of its
/// current full-screen frame (base64).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentAttachResult {
    pub session: AgentSessionInfo,
    /// Base64-encoded `vt100` screen frame.
    pub data: String,
    /// The last normalized-event sequence already folded into `data`. A client
    /// that also consumes `agent.event` / `tasks.events` can resume strictly
    /// after this cursor without replaying text that the snapshot already
    /// contains. Older payloads omit it and decode as `0`.
    #[serde(default)]
    pub seq: u64,
    /// The session's current execution plan at [`Self::seq`]. Empty when the
    /// agent has not reported one; an empty list is also a valid cleared plan.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plan: Vec<AgentPlanEntry>,
    /// The session's current advertised slash commands at [`Self::seq`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<AgentCommand>,
}

/// `agents.prompt` params: a follow-up prompt for a structured (ACP) session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentPromptParams {
    pub session_id: String,
    pub prompt: String,
}

/// `agents.prompt` result: whether the prompt turn was accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentPromptResult {
    pub accepted: bool,
}

/// `agents.cancel` params: interrupt the in-flight turn of a structured session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentCancelParams {
    pub session_id: String,
}

/// `agents.cancel` result: whether a cancellation was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentCancelResult {
    pub cancelled: bool,
}

/// `agents.attach` / `agents.close` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionIdParams {
    pub session_id: String,
}

/// `agents.close` result: the id of the closed session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentCloseResult {
    pub closed: String,
}

/// `agents.reply` params: answer the request identified by `request_id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentReplyParams {
    pub session_id: String,
    pub request_id: String,
    pub reply: InputReply,
}

/// `agents.reply` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentReplyResult {
    pub replied: bool,
}

/// `agents.reply_elicitation` params: answer a form request identified by
/// `request_id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentElicitationReplyParams {
    pub session_id: String,
    pub request_id: String,
    pub reply: ElicitationReply,
}

/// `agents.reply_elicitation` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentElicitationReplyResult {
    pub replied: bool,
}

/// `agents.options` params: read one live session's advertised selectors.
pub type AgentOptionsParams = SessionIdParams;

/// `agents.options` / `agents.set_option` / `agents.set_mode` result: the
/// complete post-change snapshot for the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentOptionsResult {
    pub session_id: String,
    #[serde(default)]
    pub config_options: Vec<SessionConfigOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modes: Option<SessionModes>,
}

/// `agents.set_option` params: change one advertised config option.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentSetOptionParams {
    pub session_id: String,
    pub option_id: String,
    pub value: SessionConfigValue,
}

/// `agents.set_mode` params: switch to one advertised mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentSetModeParams {
    pub session_id: String,
    pub mode_id: String,
}

/// `agents.provider_sessions` params: provider-owned history for one agent.
///
/// When `agent` is absent every available ACP agent is queried and one result
/// per agent is returned. `cwd` filters the provider's own `session/list`;
/// `cursor` is the opaque token from a previous page. Cursors are scoped to one
/// agent, so pagination requires an explicit `agent`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProviderSessionsParams {
    #[serde(default, deserialize_with = "lenient")]
    pub agent: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub cwd: Option<PathBuf>,
    #[serde(default, deserialize_with = "lenient")]
    pub cursor: Option<String>,
}

/// `agents.provider_sessions` result: one page (or one unsupported/error notice)
/// per queried agent.
pub type ProviderSessionsListResult = Vec<ProviderSessionsResult>;

/// `agents.provider_resume` params: resume a provider-owned session.
///
/// `session_id` is the provider's ACP id and is only accepted by this call. The
/// daemon imports the session into a Favetto-local record and returns that
/// record's id; subsequent RPCs use the local id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProviderResumeParams {
    #[serde(default, deserialize_with = "lenient")]
    pub agent: Option<String>,
    /// The provider's ACP session id. This is not a Favetto-local routing key.
    pub provider_session_id: String,
    /// Working directory to resume in. Defaults to the provider session's
    /// listed directory when omitted.
    #[serde(default, deserialize_with = "lenient")]
    pub cwd: Option<PathBuf>,
}

/// `agents.provider_delete` params: delete a provider-owned session.
///
/// This deletes the provider's transcript through ACP `session/delete`; it never
/// deletes Favetto-local metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProviderDeleteParams {
    #[serde(default, deserialize_with = "lenient")]
    pub agent: Option<String>,
    /// The provider's ACP session id. This is not a Favetto-local routing key.
    pub provider_session_id: String,
}

/// `agents.provider_delete` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProviderDeleteResult {
    pub deleted: bool,
}

/// `agents.authenticate` params: complete an agent-handled ACP v1 sign-in.
///
/// Only method ids the agent advertised during `initialize` are accepted.
/// Credentials never travel through this call: the daemon asks the agent to run
/// its own authentication flow on the daemon host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentAuthenticateParams {
    pub session_id: String,
    pub method_id: String,
}

/// `agents.authenticate` result: the session's authentication state after the
/// attempt, plus whether the daemon completed the agent-handled sign-in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentAuthenticateResult {
    pub authenticated: bool,
    pub auth: AgentAuthState,
}

/// `agents.logout` params: sign an ACP session out when the agent advertised
/// `logout`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentLogoutParams {
    pub session_id: String,
}

/// `agents.logout` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentLogoutResult {
    pub logged_out: bool,
}

// ---------------------------------------------------------------------------
// providers
// ---------------------------------------------------------------------------

/// `providers.list` params. `agent` selects an agent; the default agent is used
/// when absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProvidersListParams {
    #[serde(default, deserialize_with = "lenient")]
    pub agent: Option<String>,
}

/// A model advertised by a provider, as returned by `providers.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProviderModelInfo {
    pub id: String,
    pub name: String,
}

/// A configured provider and its available models, as returned by
/// `providers.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    pub models: Vec<ProviderModelInfo>,
}

/// `providers.list` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProvidersListResult {
    pub providers: Vec<ProviderInfo>,
}

// ---------------------------------------------------------------------------
// schedules
// ---------------------------------------------------------------------------

/// `schedules.list` result.
pub type SchedulesListResult = Vec<Schedule>;

/// `schedules.upsert` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ScheduleUpsertParams {
    #[serde(default, deserialize_with = "lenient")]
    pub id: Option<String>,
    pub cron: String,
    pub task: String,
    #[serde(default, deserialize_with = "lenient")]
    pub input: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "lenient")]
    pub enabled: Option<bool>,
}

/// `schedules.upsert` result: the stored schedule.
pub type SchedulesUpsertResult = Schedule;

/// `schedules.delete` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ScheduleDeleteParams {
    pub id: String,
}

/// `schedules.delete` result: the id of the deleted schedule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SchedulesDeleteResult {
    pub deleted: String,
}

// ---------------------------------------------------------------------------
// notifications
// ---------------------------------------------------------------------------

/// `notifications.list` params. `limit` defaults to 50 and caps at 1000.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NotificationsListParams {
    #[serde(default, deserialize_with = "lenient")]
    pub limit: Option<u64>,
}

/// `notifications.list` result.
pub type NotificationsListResult = Vec<NotificationRecord>;

/// `notifications.test` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NotificationTestParams {
    pub channel: String,
    #[serde(default, deserialize_with = "lenient")]
    pub config: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "lenient")]
    pub subject: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub body: Option<String>,
}

/// `notifications.test` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NotificationTestResult {
    pub sent: bool,
}

/// `hooks.upsert` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HookUpsertParams {
    pub event: String,
    pub channel: String,
    #[serde(default, deserialize_with = "lenient")]
    pub config: Option<serde_json::Value>,
}

/// `hooks.upsert` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HookUpsertResult {
    pub added: bool,
}

/// `usage.stats` params. `period` defaults to `day` when omitted or malformed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UsageStatsParams {
    #[serde(default, deserialize_with = "lenient")]
    pub period: Option<UsagePeriod>,
}

/// `usage.stats` result: per-bucket token/cost series plus window totals.
pub type UsageStatsResult = UsageStats;

// ---------------------------------------------------------------------------
// repos
// ---------------------------------------------------------------------------

/// `repos.list` result: known repositories plus the active repo name (if any).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ReposListResult {
    pub repos: Vec<RepoInfo>,
    #[serde(default)]
    pub active: Option<String>,
}

/// `repos.add` params: a daemon-host path and an optional display name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ReposAddParams {
    pub path: String,
    #[serde(default, deserialize_with = "lenient")]
    pub name: Option<String>,
}

/// `repos.add` result: the registered repo.
pub type ReposAddResult = RepoInfo;

/// `repos.remove` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ReposRemoveParams {
    pub name: String,
}

/// `repos.remove` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ReposRemoveResult {
    pub removed: bool,
}

/// `repos.activate` params: the repo name, or `null`/absent to clear the active
/// repo (falling back to the global catalog).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ReposActivateParams {
    #[serde(default, deserialize_with = "lenient")]
    pub name: Option<String>,
}

/// `repos.activate` result: the updated repo list.
pub type ReposActivateResult = ReposListResult;

/// `repos.refresh` result: the updated repo list after a base-directory scan.
pub type ReposRefreshResult = ReposListResult;

// ---------------------------------------------------------------------------
// push payloads
// ---------------------------------------------------------------------------

/// `catalog.updated` push: no payload.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CatalogUpdatedPush {}

/// `agent.exit` push: the session's child process exited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentExitPush {
    pub session_id: String,
    /// Process exit code, when one was reported.
    pub code: Option<i32>,
}

/// `agent.state` push: the session's folded live state changed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentStatePush {
    pub session_id: String,
    pub activity: Option<AgentActivity>,
    pub usage: Option<AgentUsage>,
}

/// `agent.event` push: one normalized agent-state event, in order. Non-durable;
/// `seq` is monotonic per session and is the feed's backfill cursor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentEventPush {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub seq: u64,
    pub event: AgentStateEvent,
}

// ---------------------------------------------------------------------------
// typed call registry
// ---------------------------------------------------------------------------

/// A client → server method whose params and result are typed.
///
/// Implemented by one marker type per method so a typed client can be generic
/// over the call: `client.call::<TasksGetCall>(params)`.
pub trait RpcCall {
    /// The wire method name.
    const METHOD: &'static str;
    /// The params payload type.
    type Params: Serialize + DeserializeOwned;
    /// The result payload type.
    type Result: Serialize + DeserializeOwned;
}

macro_rules! rpc_calls {
    ($(
        $(#[$meta:meta])*
        $call:ident => $method:path, $params:ty, $result:ty;
    )*) => {
        $(
            $(#[$meta])*
            #[derive(Debug, Clone, Copy)]
            pub struct $call;

            impl RpcCall for $call {
                const METHOD: &'static str = $method;
                type Params = $params;
                type Result = $result;
            }
        )*
    };
}

rpc_calls! {
    /// Typed call for `system.ping`.
    PingCall => method::PING, PingParams, PingResult;
    /// Typed call for `config.validate`.
    ConfigValidateCall => method::CONFIG_VALIDATE, EmptyParams, ConfigValidateResult;
    /// Typed call for `tasks.list`.
    TasksListCall => method::TASKS_LIST, TasksListParams, TasksListResult;
    /// Typed call for `tasks.get`.
    TasksGetCall => method::TASKS_GET, TaskIdParams, TasksGetResult;
    /// Typed call for `tasks.runs`.
    TasksRunsCall => method::TASKS_RUNS, TaskIdParams, TasksRunsResult;
    /// Typed call for `tasks.events`.
    TasksEventsCall => method::TASKS_EVENTS, TasksEventsParams, TasksEventsResult;
    /// Typed call for `tasks.cancel`.
    TasksCancelCall => method::TASKS_CANCEL, TaskIdParams, TasksCancelResult;
    /// Typed call for `tasks.retry`.
    TasksRetryCall => method::TASKS_RETRY, TaskIdParams, TasksRetryResult;
    /// Typed call for `tasks.start`.
    TasksStartCall => method::TASKS_START, TasksStartParams, TasksStartResult;
    /// Typed call for `tasks.start_oneshot`.
    TasksStartOneshotCall => method::TASKS_START_ONESHOT, StartOneshotParams, StartOneshotResult;
    /// Typed call for `catalog.list`.
    CatalogListCall => method::CATALOG_LIST, EmptyParams, CatalogListResult;
    /// Typed call for `catalog.get`.
    CatalogGetCall => method::CATALOG_GET, CatalogGetParams, CatalogGetResult;
    /// Typed call for `catalog.add`.
    CatalogAddCall => method::CATALOG_ADD, CatalogAddParams, CatalogAddResult;
    /// Typed call for `catalog.update`.
    CatalogUpdateCall => method::CATALOG_UPDATE, CatalogUpdateParams, CatalogUpdateResult;
    /// Typed call for `workflow.get`.
    WorkflowGetCall => method::WORKFLOW_GET, EmptyParams, WorkflowGetResult;
    /// Typed call for `workflow.inspect`.
    WorkflowInspectCall => method::WORKFLOW_INSPECT, WorkflowInspectParams, WorkflowInspectResult;
    /// Typed call for `workflow.create`.
    WorkflowCreateCall => method::WORKFLOW_CREATE, WorkflowCreateParams, WorkflowCreateResultBody;
    /// Typed call for `workflow.spawn`.
    WorkflowSpawnCall => method::WORKFLOW_SPAWN, WorkflowSpawnParams, WorkflowSpawnResult;
    /// Typed call for `workflow.cancel`.
    WorkflowCancelCall => method::WORKFLOW_CANCEL, WorkflowInspectParams, WorkflowCancelResultBody;
    /// Typed call for `workflow.retry`.
    WorkflowRetryCall => method::WORKFLOW_RETRY, WorkflowRetryParams, WorkflowRetryResult;
    /// Typed call for `events.tail`.
    EventsTailCall => method::EVENTS_TAIL, EventsTailParams, EventsTailResult;
    /// Typed call for `events.subscribe`.
    EventsSubscribeCall => method::EVENTS_SUBSCRIBE, EventsSubscribeParams, EventsSubscribeResult;
    /// Typed call for `agents.list`.
    AgentsListCall => method::AGENTS_LIST, EmptyParams, AgentsListResult;
    /// Typed call for `agents.history`.
    AgentsHistoryCall => method::AGENTS_HISTORY, AgentsHistoryParams, AgentsHistoryResult;
    /// Typed call for `agents.start`.
    AgentsStartCall => method::AGENTS_START, StartAgentParams, AgentAttachResult;
    /// Typed call for `agents.load`.
    AgentsLoadCall => method::AGENTS_LOAD, AgentLoadParams, AgentAttachResult;
    /// Typed call for `agents.prompt`.
    AgentsPromptCall => method::AGENTS_PROMPT, AgentPromptParams, AgentPromptResult;
    /// Typed call for `agents.cancel`.
    AgentsCancelCall => method::AGENTS_CANCEL, AgentCancelParams, AgentCancelResult;
    /// Typed call for `agents.attach`.
    AgentsAttachCall => method::AGENTS_ATTACH, SessionIdParams, AgentAttachResult;
    /// Typed call for `agents.close`.
    AgentsCloseCall => method::AGENTS_CLOSE, SessionIdParams, AgentCloseResult;
    /// Typed call for `agents.reply`.
    AgentsReplyCall => method::AGENTS_REPLY, AgentReplyParams, AgentReplyResult;
    /// Typed call for `agents.reply_elicitation`.
    AgentsReplyElicitationCall => method::AGENTS_REPLY_ELICITATION, AgentElicitationReplyParams, AgentElicitationReplyResult;
    /// Typed call for `agents.options`.
    AgentsOptionsCall => method::AGENTS_OPTIONS, AgentOptionsParams, AgentOptionsResult;
    /// Typed call for `agents.set_option`.
    AgentsSetOptionCall => method::AGENTS_SET_OPTION, AgentSetOptionParams, AgentOptionsResult;
    /// Typed call for `agents.set_mode`.
    AgentsSetModeCall => method::AGENTS_SET_MODE, AgentSetModeParams, AgentOptionsResult;
    /// Typed call for `agents.provider_sessions`.
    AgentsProviderSessionsCall => method::AGENTS_PROVIDER_SESSIONS, ProviderSessionsParams, ProviderSessionsListResult;
    /// Typed call for `agents.provider_resume`.
    AgentsProviderResumeCall => method::AGENTS_PROVIDER_RESUME, ProviderResumeParams, AgentAttachResult;
    /// Typed call for `agents.provider_delete`.
    AgentsProviderDeleteCall => method::AGENTS_PROVIDER_DELETE, ProviderDeleteParams, ProviderDeleteResult;
    /// Typed call for `agents.authenticate`.
    AgentsAuthenticateCall => method::AGENTS_AUTHENTICATE, AgentAuthenticateParams, AgentAuthenticateResult;
    /// Typed call for `agents.logout`.
    AgentsLogoutCall => method::AGENTS_LOGOUT, AgentLogoutParams, AgentLogoutResult;
    /// Typed call for `providers.list`.
    ProvidersListCall => method::PROVIDERS_LIST, ProvidersListParams, ProvidersListResult;
    /// Typed call for `schedules.list`.
    SchedulesListCall => method::SCHEDULES_LIST, EmptyParams, SchedulesListResult;
    /// Typed call for `schedules.upsert`.
    SchedulesUpsertCall => method::SCHEDULES_UPSERT, ScheduleUpsertParams, SchedulesUpsertResult;
    /// Typed call for `schedules.delete`.
    SchedulesDeleteCall => method::SCHEDULES_DELETE, ScheduleDeleteParams, SchedulesDeleteResult;
    /// Typed call for `notifications.list`.
    NotificationsListCall => method::NOTIFICATIONS_LIST, NotificationsListParams, NotificationsListResult;
    /// Typed call for `notifications.test`.
    NotificationsTestCall => method::NOTIFICATIONS_TEST, NotificationTestParams, NotificationTestResult;
    /// Typed call for `hooks.upsert`.
    HooksUpsertCall => method::HOOKS_UPSERT, HookUpsertParams, HookUpsertResult;
    /// Typed call for `usage.stats`.
    UsageStatsCall => method::USAGE_STATS, UsageStatsParams, UsageStatsResult;
    /// Typed call for `repos.list`.
    ReposListCall => method::REPOS_LIST, EmptyParams, ReposListResult;
    /// Typed call for `repos.add`.
    ReposAddCall => method::REPOS_ADD, ReposAddParams, ReposAddResult;
    /// Typed call for `repos.remove`.
    ReposRemoveCall => method::REPOS_REMOVE, ReposRemoveParams, ReposRemoveResult;
    /// Typed call for `repos.activate`.
    ReposActivateCall => method::REPOS_ACTIVATE, ReposActivateParams, ReposActivateResult;
    /// Typed call for `repos.refresh`.
    ReposRefreshCall => method::REPOS_REFRESH, EmptyParams, ReposRefreshResult;
}

/// The JSON Schema of a type, as a plain JSON value. Used by the doc generator.
#[cfg(feature = "schema")]
pub fn schema_of<T: JsonSchema>() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or(serde_json::Value::Null)
}

/// One client → server method's typed metadata, consumed by the generated
/// remote-API reference.
#[cfg(feature = "schema")]
pub struct RpcCallSpec {
    /// The wire method name.
    pub method: &'static str,
    /// Returns the JSON Schema of the params type.
    pub params_schema: fn() -> serde_json::Value,
    /// Returns the JSON Schema of the result type.
    pub result_schema: fn() -> serde_json::Value,
}

/// Every client → server call with its params and result schema, in the same
/// order as [`CLIENT_METHODS`](super::CLIENT_METHODS).
#[cfg(feature = "schema")]
pub const CLIENT_CALLS: &[RpcCallSpec] = &[
    RpcCallSpec {
        method: method::PING,
        params_schema: schema_of::<PingParams>,
        result_schema: schema_of::<PingResult>,
    },
    RpcCallSpec {
        method: method::CONFIG_VALIDATE,
        params_schema: schema_of::<EmptyParams>,
        result_schema: schema_of::<ConfigValidateResult>,
    },
    RpcCallSpec {
        method: method::TASKS_LIST,
        params_schema: schema_of::<TasksListParams>,
        result_schema: schema_of::<TasksListResult>,
    },
    RpcCallSpec {
        method: method::TASKS_GET,
        params_schema: schema_of::<TaskIdParams>,
        result_schema: schema_of::<TasksGetResult>,
    },
    RpcCallSpec {
        method: method::TASKS_RUNS,
        params_schema: schema_of::<TaskIdParams>,
        result_schema: schema_of::<TasksRunsResult>,
    },
    RpcCallSpec {
        method: method::TASKS_EVENTS,
        params_schema: schema_of::<TasksEventsParams>,
        result_schema: schema_of::<TasksEventsResult>,
    },
    RpcCallSpec {
        method: method::TASKS_CANCEL,
        params_schema: schema_of::<TaskIdParams>,
        result_schema: schema_of::<TasksCancelResult>,
    },
    RpcCallSpec {
        method: method::TASKS_RETRY,
        params_schema: schema_of::<TaskIdParams>,
        result_schema: schema_of::<TasksRetryResult>,
    },
    RpcCallSpec {
        method: method::TASKS_START,
        params_schema: schema_of::<TasksStartParams>,
        result_schema: schema_of::<TasksStartResult>,
    },
    RpcCallSpec {
        method: method::TASKS_START_ONESHOT,
        params_schema: schema_of::<StartOneshotParams>,
        result_schema: schema_of::<StartOneshotResult>,
    },
    RpcCallSpec {
        method: method::CATALOG_LIST,
        params_schema: schema_of::<EmptyParams>,
        result_schema: schema_of::<CatalogListResult>,
    },
    RpcCallSpec {
        method: method::CATALOG_GET,
        params_schema: schema_of::<CatalogGetParams>,
        result_schema: schema_of::<CatalogGetResult>,
    },
    RpcCallSpec {
        method: method::CATALOG_ADD,
        params_schema: schema_of::<CatalogAddParams>,
        result_schema: schema_of::<CatalogAddResult>,
    },
    RpcCallSpec {
        method: method::CATALOG_UPDATE,
        params_schema: schema_of::<CatalogUpdateParams>,
        result_schema: schema_of::<CatalogUpdateResult>,
    },
    RpcCallSpec {
        method: method::WORKFLOW_GET,
        params_schema: schema_of::<EmptyParams>,
        result_schema: schema_of::<WorkflowGetResult>,
    },
    RpcCallSpec {
        method: method::WORKFLOW_INSPECT,
        params_schema: schema_of::<WorkflowInspectParams>,
        result_schema: schema_of::<WorkflowInspectResult>,
    },
    RpcCallSpec {
        method: method::WORKFLOW_CREATE,
        params_schema: schema_of::<WorkflowCreateParams>,
        result_schema: schema_of::<WorkflowCreateResultBody>,
    },
    RpcCallSpec {
        method: method::WORKFLOW_SPAWN,
        params_schema: schema_of::<WorkflowSpawnParams>,
        result_schema: schema_of::<WorkflowSpawnResult>,
    },
    RpcCallSpec {
        method: method::WORKFLOW_CANCEL,
        params_schema: schema_of::<WorkflowInspectParams>,
        result_schema: schema_of::<WorkflowCancelResultBody>,
    },
    RpcCallSpec {
        method: method::WORKFLOW_RETRY,
        params_schema: schema_of::<WorkflowRetryParams>,
        result_schema: schema_of::<WorkflowRetryResult>,
    },
    RpcCallSpec {
        method: method::EVENTS_TAIL,
        params_schema: schema_of::<EventsTailParams>,
        result_schema: schema_of::<EventsTailResult>,
    },
    RpcCallSpec {
        method: method::EVENTS_SUBSCRIBE,
        params_schema: schema_of::<EventsSubscribeParams>,
        result_schema: schema_of::<EventsSubscribeResult>,
    },
    RpcCallSpec {
        method: method::AGENTS_LIST,
        params_schema: schema_of::<EmptyParams>,
        result_schema: schema_of::<AgentsListResult>,
    },
    RpcCallSpec {
        method: method::AGENTS_HISTORY,
        params_schema: schema_of::<AgentsHistoryParams>,
        result_schema: schema_of::<AgentsHistoryResult>,
    },
    RpcCallSpec {
        method: method::AGENTS_START,
        params_schema: schema_of::<StartAgentParams>,
        result_schema: schema_of::<AgentAttachResult>,
    },
    RpcCallSpec {
        method: method::AGENTS_LOAD,
        params_schema: schema_of::<AgentLoadParams>,
        result_schema: schema_of::<AgentAttachResult>,
    },
    RpcCallSpec {
        method: method::AGENTS_PROMPT,
        params_schema: schema_of::<AgentPromptParams>,
        result_schema: schema_of::<AgentPromptResult>,
    },
    RpcCallSpec {
        method: method::AGENTS_CANCEL,
        params_schema: schema_of::<AgentCancelParams>,
        result_schema: schema_of::<AgentCancelResult>,
    },
    RpcCallSpec {
        method: method::AGENTS_ATTACH,
        params_schema: schema_of::<SessionIdParams>,
        result_schema: schema_of::<AgentAttachResult>,
    },
    RpcCallSpec {
        method: method::AGENTS_CLOSE,
        params_schema: schema_of::<SessionIdParams>,
        result_schema: schema_of::<AgentCloseResult>,
    },
    RpcCallSpec {
        method: method::AGENTS_REPLY,
        params_schema: schema_of::<AgentReplyParams>,
        result_schema: schema_of::<AgentReplyResult>,
    },
    RpcCallSpec {
        method: method::AGENTS_REPLY_ELICITATION,
        params_schema: schema_of::<AgentElicitationReplyParams>,
        result_schema: schema_of::<AgentElicitationReplyResult>,
    },
    RpcCallSpec {
        method: method::AGENTS_OPTIONS,
        params_schema: schema_of::<AgentOptionsParams>,
        result_schema: schema_of::<AgentOptionsResult>,
    },
    RpcCallSpec {
        method: method::AGENTS_SET_OPTION,
        params_schema: schema_of::<AgentSetOptionParams>,
        result_schema: schema_of::<AgentOptionsResult>,
    },
    RpcCallSpec {
        method: method::AGENTS_SET_MODE,
        params_schema: schema_of::<AgentSetModeParams>,
        result_schema: schema_of::<AgentOptionsResult>,
    },
    RpcCallSpec {
        method: method::AGENTS_PROVIDER_SESSIONS,
        params_schema: schema_of::<ProviderSessionsParams>,
        result_schema: schema_of::<ProviderSessionsListResult>,
    },
    RpcCallSpec {
        method: method::AGENTS_PROVIDER_RESUME,
        params_schema: schema_of::<ProviderResumeParams>,
        result_schema: schema_of::<AgentAttachResult>,
    },
    RpcCallSpec {
        method: method::AGENTS_PROVIDER_DELETE,
        params_schema: schema_of::<ProviderDeleteParams>,
        result_schema: schema_of::<ProviderDeleteResult>,
    },
    RpcCallSpec {
        method: method::AGENTS_AUTHENTICATE,
        params_schema: schema_of::<AgentAuthenticateParams>,
        result_schema: schema_of::<AgentAuthenticateResult>,
    },
    RpcCallSpec {
        method: method::AGENTS_LOGOUT,
        params_schema: schema_of::<AgentLogoutParams>,
        result_schema: schema_of::<AgentLogoutResult>,
    },
    RpcCallSpec {
        method: method::PROVIDERS_LIST,
        params_schema: schema_of::<ProvidersListParams>,
        result_schema: schema_of::<ProvidersListResult>,
    },
    RpcCallSpec {
        method: method::SCHEDULES_LIST,
        params_schema: schema_of::<EmptyParams>,
        result_schema: schema_of::<SchedulesListResult>,
    },
    RpcCallSpec {
        method: method::SCHEDULES_UPSERT,
        params_schema: schema_of::<ScheduleUpsertParams>,
        result_schema: schema_of::<SchedulesUpsertResult>,
    },
    RpcCallSpec {
        method: method::SCHEDULES_DELETE,
        params_schema: schema_of::<ScheduleDeleteParams>,
        result_schema: schema_of::<SchedulesDeleteResult>,
    },
    RpcCallSpec {
        method: method::NOTIFICATIONS_LIST,
        params_schema: schema_of::<NotificationsListParams>,
        result_schema: schema_of::<NotificationsListResult>,
    },
    RpcCallSpec {
        method: method::NOTIFICATIONS_TEST,
        params_schema: schema_of::<NotificationTestParams>,
        result_schema: schema_of::<NotificationTestResult>,
    },
    RpcCallSpec {
        method: method::HOOKS_UPSERT,
        params_schema: schema_of::<HookUpsertParams>,
        result_schema: schema_of::<HookUpsertResult>,
    },
    RpcCallSpec {
        method: method::USAGE_STATS,
        params_schema: schema_of::<UsageStatsParams>,
        result_schema: schema_of::<UsageStatsResult>,
    },
    RpcCallSpec {
        method: method::REPOS_LIST,
        params_schema: schema_of::<EmptyParams>,
        result_schema: schema_of::<ReposListResult>,
    },
    RpcCallSpec {
        method: method::REPOS_ADD,
        params_schema: schema_of::<ReposAddParams>,
        result_schema: schema_of::<ReposAddResult>,
    },
    RpcCallSpec {
        method: method::REPOS_REMOVE,
        params_schema: schema_of::<ReposRemoveParams>,
        result_schema: schema_of::<ReposRemoveResult>,
    },
    RpcCallSpec {
        method: method::REPOS_ACTIVATE,
        params_schema: schema_of::<ReposActivateParams>,
        result_schema: schema_of::<ReposActivateResult>,
    },
    RpcCallSpec {
        method: method::REPOS_REFRESH,
        params_schema: schema_of::<EmptyParams>,
        result_schema: schema_of::<ReposRefreshResult>,
    },
];

/// One server → client push's typed metadata.
#[cfg(feature = "schema")]
pub struct PushSpec {
    /// The wire method name.
    pub method: &'static str,
    /// Returns the JSON Schema of the params (push payload) type.
    pub params_schema: fn() -> serde_json::Value,
}

/// Every server → client push with its payload schema, in the same order as
/// [`SERVER_PUSHES`](super::SERVER_PUSHES).
#[cfg(feature = "schema")]
pub const PUSH_CALLS: &[PushSpec] = &[
    PushSpec {
        method: super::push::EVENT,
        params_schema: schema_of::<Event>,
    },
    PushSpec {
        method: super::push::TASK_UPDATED,
        params_schema: schema_of::<Task>,
    },
    PushSpec {
        method: super::push::CATALOG_UPDATED,
        params_schema: schema_of::<CatalogUpdatedPush>,
    },
    PushSpec {
        method: super::push::AGENT_EXIT,
        params_schema: schema_of::<AgentExitPush>,
    },
    PushSpec {
        method: super::push::AGENT_STATE,
        params_schema: schema_of::<AgentStatePush>,
    },
    PushSpec {
        method: super::push::AGENT_EVENT,
        params_schema: schema_of::<AgentEventPush>,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "schema")]
    use std::collections::HashSet;

    #[cfg(feature = "schema")]
    #[test]
    fn client_calls_and_methods_table_agree() {
        let calls: HashSet<&str> = CLIENT_CALLS.iter().map(|c| c.method).collect();
        let table: HashSet<&str> = super::super::CLIENT_METHODS
            .iter()
            .map(|(m, _)| *m)
            .collect();
        assert_eq!(calls, table);
        assert_eq!(CLIENT_CALLS.len(), super::super::CLIENT_METHODS.len());
    }

    #[cfg(feature = "schema")]
    #[test]
    fn push_calls_and_pushes_table_agree() {
        let calls: HashSet<&str> = PUSH_CALLS.iter().map(|c| c.method).collect();
        let table: HashSet<&str> = super::super::SERVER_PUSHES
            .iter()
            .map(|(m, _)| *m)
            .collect();
        assert_eq!(calls, table);
        assert_eq!(PUSH_CALLS.len(), super::super::SERVER_PUSHES.len());
    }

    #[cfg(feature = "schema")]
    #[test]
    fn every_call_has_a_schema_without_duplicate_methods() {
        let mut seen = HashSet::new();
        for call in CLIENT_CALLS {
            assert!(seen.insert(call.method), "duplicate {}", call.method);
            assert!(
                (call.params_schema)().is_object(),
                "{} params schema",
                call.method
            );
            assert!(
                (call.result_schema)().is_object(),
                "{} result schema",
                call.method
            );
        }
        for push in PUSH_CALLS {
            assert!(
                (push.params_schema)().is_object(),
                "{} push schema",
                push.method
            );
        }
    }

    #[test]
    fn lenient_optional_fields_default_to_none() {
        let p: TasksListParams =
            serde_json::from_value(serde_json::json!({ "limit": "many" })).unwrap();
        assert_eq!(p.limit, None);
        let p: TasksListParams = serde_json::from_value(serde_json::json!({ "limit": 7 })).unwrap();
        assert_eq!(p.limit, Some(7));

        // `usage.stats` accepts a missing or ill-typed period.
        let p: UsageStatsParams = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(p.period, None);
        let p: UsageStatsParams =
            serde_json::from_value(serde_json::json!({ "period": "month" })).unwrap();
        assert_eq!(p.period, Some(UsagePeriod::Month));
        let p: UsageStatsParams =
            serde_json::from_value(serde_json::json!({ "period": 3 })).unwrap();
        assert_eq!(p.period, None);
    }

    #[test]
    fn agent_event_and_tasks_events_round_trip() {
        let push = AgentEventPush {
            session_id: "sess-1".to_string(),
            task_id: Some("task-1".to_string()),
            seq: 3,
            event: AgentStateEvent::TextDelta {
                role: crate::model::MessageRole::Assistant,
                text: "hi".to_string(),
            },
        };
        let json = serde_json::to_value(&push).unwrap();
        assert_eq!(json["event"]["event"], "text_delta");
        assert_eq!(json["event"]["role"], "assistant");
        let back: AgentEventPush = serde_json::from_value(json).unwrap();
        assert_eq!(back, push);

        // `task_id` is omitted when absent.
        let lean = serde_json::to_value(AgentEventPush {
            task_id: None,
            ..push.clone()
        })
        .unwrap();
        assert!(lean.get("task_id").is_none());

        let result = TasksEventsResult {
            session_id: Some("sess-1".to_string()),
            events: vec![push],
            next_cursor: 3,
            plan: Some(vec![AgentPlanEntry {
                content: "step".to_string(),
                ..Default::default()
            }]),
            commands: Some(vec![AgentCommand {
                name: "web".to_string(),
                description: "Search".to_string(),
                input_hint: Some("query".to_string()),
            }]),
        };
        let json = serde_json::to_value(&result).unwrap();
        let back: TasksEventsResult = serde_json::from_value(json).unwrap();
        assert_eq!(back, result);

        // A legacy payload has no plan/commands fields and keeps the
        // "no structured session" distinction.
        let legacy: TasksEventsResult = serde_json::from_value(serde_json::json!({
            "events": [],
            "next_cursor": 0
        }))
        .unwrap();
        assert_eq!(legacy.plan, None);
        assert_eq!(legacy.commands, None);

        // `after`/`limit` accept missing or ill-typed values.
        let p: TasksEventsParams =
            serde_json::from_value(serde_json::json!({ "id": Uuid::nil() })).unwrap();
        assert_eq!(p.after, None);
        assert_eq!(p.limit, None);
        let p: TasksEventsParams =
            serde_json::from_value(serde_json::json!({ "id": Uuid::nil(), "after": "x" })).unwrap();
        assert_eq!(p.after, None);
    }

    #[test]
    fn attach_result_seq_is_additive_on_the_wire() {
        // Older daemons omit `seq`; newer clients decode it as zero and keep the
        // rest of the attach contract intact.
        let legacy: AgentAttachResult = serde_json::from_value(serde_json::json!({
            "session": {
                "id": "s1",
                "agent": "opencode",
                "task_id": null,
                "running": true,
                "headless": false
            },
            "data": ""
        }))
        .unwrap();
        assert_eq!(legacy.seq, 0);
        assert!(legacy.plan.is_empty());
        assert!(legacy.commands.is_empty());
        assert_eq!(legacy.session.id, "s1");

        let current = AgentAttachResult {
            seq: 7,
            plan: vec![AgentPlanEntry {
                content: "step".to_string(),
                ..Default::default()
            }],
            commands: vec![AgentCommand {
                name: "test".to_string(),
                description: "Run tests".to_string(),
                input_hint: None,
            }],
            ..legacy.clone()
        };
        let json = serde_json::to_value(&current).unwrap();
        assert_eq!(json["seq"], 7);
        assert_eq!(json["plan"][0]["content"], "step");
        assert_eq!(json["commands"][0]["name"], "test");
        let back: AgentAttachResult = serde_json::from_value(json).unwrap();
        assert_eq!(back.seq, 7);
        assert_eq!(back.plan, current.plan);
        assert_eq!(back.commands, current.commands);
        assert_eq!(back.session.load_state, crate::model::AgentLoadState::Ready);
    }

    #[test]
    fn result_shapes_match_the_wire_contract() {
        // `catalog.add` omits `prompt`/`spawn*` whereas `catalog.list` includes
        // `prompt`; keep verifying the shapes the daemon serializes.
        let entry = CatalogEntry {
            name: "t".to_string(),
            agent: None,
            provider: None,
            model: None,
            cwd: None,
            schedule: None,
            kind: "action".to_string(),
            needs: None,
            vars: Vec::new(),
            prompt: "p".to_string(),
            repo: None,
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["agent"], serde_json::Value::Null);
        assert_eq!(json["prompt"], "p");
        assert_eq!(json["repo"], serde_json::Value::Null);
        assert_eq!(json["kind"], "action");

        let add = CatalogAddResult {
            name: "t".to_string(),
            agent: None,
            provider: None,
            model: None,
            cwd: None,
            schedule: None,
            needs: None,
            vars: Vec::new(),
        };
        let json = serde_json::to_value(&add).unwrap();
        assert!(json.get("prompt").is_none());
        assert!(json.get("spawn").is_none());

        let ping = serde_json::to_value(PingResult {
            pong: true,
            cwd: "/tmp".to_string(),
        })
        .unwrap();
        assert_eq!(ping, serde_json::json!({ "pong": true, "cwd": "/tmp" }));
    }

    #[test]
    fn catalog_entry_kind_round_trips_and_defaults_empty() {
        let entry = CatalogEntry {
            name: "t".to_string(),
            agent: None,
            provider: None,
            model: None,
            cwd: None,
            schedule: None,
            kind: "agent".to_string(),
            needs: None,
            vars: Vec::new(),
            prompt: "p".to_string(),
            repo: None,
        };
        let json = serde_json::to_value(&entry).unwrap();
        let back: CatalogEntry = serde_json::from_value(json).unwrap();
        assert_eq!(back, entry);
        assert_eq!(back.kind, "agent");

        // A daemon that predates `kind` omits the field; it defaults to empty so
        // clients fall back to their own `agent` default.
        let legacy: CatalogEntry = serde_json::from_value(serde_json::json!({
            "name": "t",
            "agent": null,
            "provider": null,
            "model": null,
            "cwd": null,
            "schedule": null,
            "needs": null,
            "vars": [],
            "prompt": "p",
            "repo": null,
        }))
        .unwrap();
        assert_eq!(legacy.kind, "");
    }

    #[test]
    fn config_validate_result_round_trips() {
        let result = ConfigValidateResult {
            valid: false,
            path: "/home/u/.config/favetto/config.toml".to_string(),
            errors: vec!["invalid type: string, expected a boolean".to_string()],
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "valid": false,
                "path": "/home/u/.config/favetto/config.toml",
                "errors": ["invalid type: string, expected a boolean"],
            })
        );
        let back: ConfigValidateResult = serde_json::from_value(json).unwrap();
        assert_eq!(back, result);
    }
}
