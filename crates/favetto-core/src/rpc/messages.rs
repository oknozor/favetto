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

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::agent_state::{AgentActivity, AgentUsage, InputReply, UsagePeriod, UsageStats};
use crate::model::{
    AgentCatalogEntry, AgentSessionInfo, Event, NotificationRecord, Schedule, Task,
};
use crate::tasks::TaskVar;
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EmptyParams {}

// ---------------------------------------------------------------------------
// system
// ---------------------------------------------------------------------------

/// `system.ping` params: none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PingParams {}

/// `system.ping` result: daemon liveness plus its current working directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PingResult {
    /// Always `true` for a live daemon.
    pub pong: bool,
    /// The daemon process's current working directory.
    pub cwd: String,
}

// ---------------------------------------------------------------------------
// tasks
// ---------------------------------------------------------------------------

/// `tasks.list` params. `limit` defaults to 500 and caps at 2000.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TasksListParams {
    #[serde(default, deserialize_with = "lenient")]
    pub limit: Option<u64>,
}

/// `tasks.list` result: task summaries, newest first. Output blobs are omitted;
/// fetch them with `tasks.get`.
pub type TasksListResult = Vec<Task>;

/// `tasks.get` / `tasks.cancel` / `tasks.retry` params.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TaskIdParams {
    pub id: Uuid,
}

/// `tasks.get` result: the full task, including its stored output blob.
pub type TasksGetResult = Task;

/// `tasks.cancel` result: the cancelled task, with `output` omitted.
pub type TasksCancelResult = Task;

/// `tasks.retry` result: the re-enqueued task, with `output` omitted.
pub type TasksRetryResult = Task;

/// `tasks.start` params. `input` defaults to JSON null; `interactive` defaults
/// to false (headless), so only the TUI opts into the live embedded TUI.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TasksStartParams {
    pub name: String,
    #[serde(default, deserialize_with = "lenient")]
    pub input: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "lenient")]
    pub interactive: Option<bool>,
}

/// `tasks.start` result: the enqueued task.
pub type TasksStartResult = Task;

/// `tasks.start_oneshot` params. `rows`/`cols` default to 24/80.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StartOneshotResult {
    pub task: Task,
    pub session: AgentSessionInfo,
    /// Base64-encoded `vt100` screen frame.
    pub data: String,
}

// ---------------------------------------------------------------------------
// catalog
// ---------------------------------------------------------------------------

/// A catalog task as listed by `catalog.list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CatalogEntry {
    pub name: String,
    pub agent: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub schedule: Option<String>,
    pub needs: Option<String>,
    /// Manual input variables declared with `[[vars]]`.
    pub vars: Vec<TaskVar>,
    /// The raw prompt template.
    pub prompt: String,
}

/// `catalog.list` result: every catalog definition, sorted by name.
pub type CatalogListResult = Vec<CatalogEntry>;

/// `catalog.get` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CatalogGetParams {
    pub name: String,
}

/// `catalog.get` result: the task's raw Markdown source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CatalogGetResult {
    pub markdown: String,
}

/// `catalog.add` params. `spawn_new_root` defaults to false, `prompt` to "".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CatalogUpdateParams {
    pub name: String,
    pub markdown: String,
}

/// `catalog.update` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CatalogUpdateResult {
    pub updated: bool,
}

// ---------------------------------------------------------------------------
// workflow
// ---------------------------------------------------------------------------

/// `workflow.get` result: the catalog graph as Graphviz DOT, its persisted path,
/// and the structured graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowGetResult {
    pub dot: String,
    pub path: String,
    pub graph: WorkflowGraph,
}

/// `workflow.inspect` / `workflow.cancel` params.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowInspectParams {
    pub root_id: Uuid,
}

/// `workflow.inspect` result: the runtime graph for the root.
pub type WorkflowInspectResult = WorkflowInspect;

/// `workflow.retry` params. The workflow API names the task id `task_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowRetryParams {
    pub task_id: Uuid,
}

/// One node of a `workflow.create` request. `key` is local to the request and is
/// what sibling nodes reference from `depends_on`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EventsTailParams {
    #[serde(default, deserialize_with = "lenient")]
    pub limit: Option<u64>,
}

/// `events.tail` result: the most recent persisted events.
pub type EventsTailResult = Vec<Event>;

/// `events.subscribe` params. A malformed request degrades to "no replay".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EventsSubscribeParams {
    #[serde(default, deserialize_with = "lenient")]
    pub last_event_id: Option<i64>,
}

/// `events.subscribe` result: acknowledgement of the subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EventsSubscribeResult {
    pub subscribed: bool,
}

// ---------------------------------------------------------------------------
// agents
// ---------------------------------------------------------------------------

/// `agents.list` result: configured agents plus their live sessions.
pub type AgentsListResult = Vec<AgentCatalogEntry>;

/// `agents.start` params. `rows`/`cols` default to 24/80, `new` to false.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentAttachResult {
    pub session: AgentSessionInfo,
    /// Base64-encoded `vt100` screen frame.
    pub data: String,
}

/// `agents.input` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentInputParams {
    pub session_id: String,
    #[serde(default, deserialize_with = "lenient")]
    pub data: Option<String>,
}

/// `agents.input` result: the number of decoded bytes written to the PTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentInputResult {
    pub bytes: usize,
}

/// `agents.resize` params. `rows`/`cols` default to 24/80.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentResizeParams {
    pub session_id: String,
    #[serde(default, deserialize_with = "lenient")]
    pub rows: Option<u64>,
    #[serde(default, deserialize_with = "lenient")]
    pub cols: Option<u64>,
}

/// `agents.resize` result: the applied size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentResizeResult {
    pub rows: u16,
    pub cols: u16,
}

/// `agents.attach` / `agents.close` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionIdParams {
    pub session_id: String,
}

/// `agents.close` result: the id of the closed session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentCloseResult {
    pub closed: String,
}

/// `agents.reply` params: answer the request identified by `request_id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentReplyParams {
    pub session_id: String,
    pub request_id: String,
    pub reply: InputReply,
}

/// `agents.reply` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentReplyResult {
    pub replied: bool,
}

// ---------------------------------------------------------------------------
// providers
// ---------------------------------------------------------------------------

/// `providers.list` params. `agent` selects an agent; the default agent is used
/// when absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProvidersListParams {
    #[serde(default, deserialize_with = "lenient")]
    pub agent: Option<String>,
}

/// A model advertised by a provider, as returned by `providers.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderModelInfo {
    pub id: String,
    pub name: String,
}

/// A configured provider and its available models, as returned by
/// `providers.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    pub models: Vec<ProviderModelInfo>,
}

/// `providers.list` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProvidersListResult {
    pub providers: Vec<ProviderInfo>,
}

// ---------------------------------------------------------------------------
// schedules
// ---------------------------------------------------------------------------

/// `schedules.list` result.
pub type SchedulesListResult = Vec<Schedule>;

/// `schedules.upsert` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ScheduleDeleteParams {
    pub id: String,
}

/// `schedules.delete` result: the id of the deleted schedule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SchedulesDeleteResult {
    pub deleted: String,
}

// ---------------------------------------------------------------------------
// notifications
// ---------------------------------------------------------------------------

/// `notifications.list` params. `limit` defaults to 50 and caps at 1000.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NotificationsListParams {
    #[serde(default, deserialize_with = "lenient")]
    pub limit: Option<u64>,
}

/// `notifications.list` result.
pub type NotificationsListResult = Vec<NotificationRecord>;

/// `notifications.test` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NotificationTestResult {
    pub sent: bool,
}

/// `hooks.upsert` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HookUpsertParams {
    pub event: String,
    pub channel: String,
    #[serde(default, deserialize_with = "lenient")]
    pub config: Option<serde_json::Value>,
}

/// `hooks.upsert` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HookUpsertResult {
    pub added: bool,
}

/// `usage.stats` params. `period` defaults to `day` when omitted or malformed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UsageStatsParams {
    #[serde(default, deserialize_with = "lenient")]
    pub period: Option<UsagePeriod>,
}

/// `usage.stats` result: per-bucket token/cost series plus window totals.
pub type UsageStatsResult = UsageStats;

// ---------------------------------------------------------------------------
// push payloads
// ---------------------------------------------------------------------------

/// `catalog.updated` push: no payload.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CatalogUpdatedPush {}

/// `agent.output` push: raw PTY screen data (base64).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentOutputPush {
    pub session_id: String,
    /// Base64-encoded `vt100` screen frame.
    pub data: String,
}

/// `agent.exit` push: the session's child process exited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentExitPush {
    pub session_id: String,
    /// Process exit code, when one was reported.
    pub code: Option<i32>,
}

/// `agent.state` push: the session's folded live state changed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentStatePush {
    pub session_id: String,
    pub activity: Option<AgentActivity>,
    pub usage: Option<AgentUsage>,
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
    /// Typed call for `tasks.list`.
    TasksListCall => method::TASKS_LIST, TasksListParams, TasksListResult;
    /// Typed call for `tasks.get`.
    TasksGetCall => method::TASKS_GET, TaskIdParams, TasksGetResult;
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
    /// Typed call for `agents.start`.
    AgentsStartCall => method::AGENTS_START, StartAgentParams, AgentAttachResult;
    /// Typed call for `agents.input`.
    AgentsInputCall => method::AGENTS_INPUT, AgentInputParams, AgentInputResult;
    /// Typed call for `agents.resize`.
    AgentsResizeCall => method::AGENTS_RESIZE, AgentResizeParams, AgentResizeResult;
    /// Typed call for `agents.attach`.
    AgentsAttachCall => method::AGENTS_ATTACH, SessionIdParams, AgentAttachResult;
    /// Typed call for `agents.close`.
    AgentsCloseCall => method::AGENTS_CLOSE, SessionIdParams, AgentCloseResult;
    /// Typed call for `agents.reply`.
    AgentsReplyCall => method::AGENTS_REPLY, AgentReplyParams, AgentReplyResult;
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
}

/// The JSON Schema of a type, as a plain JSON value. Used by the doc generator.
pub fn schema_of<T: JsonSchema>() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or(serde_json::Value::Null)
}

/// One client → server method's typed metadata, consumed by the generated
/// remote-API reference.
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
pub const CLIENT_CALLS: &[RpcCallSpec] = &[
    RpcCallSpec {
        method: method::PING,
        params_schema: schema_of::<PingParams>,
        result_schema: schema_of::<PingResult>,
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
        method: method::AGENTS_START,
        params_schema: schema_of::<StartAgentParams>,
        result_schema: schema_of::<AgentAttachResult>,
    },
    RpcCallSpec {
        method: method::AGENTS_INPUT,
        params_schema: schema_of::<AgentInputParams>,
        result_schema: schema_of::<AgentInputResult>,
    },
    RpcCallSpec {
        method: method::AGENTS_RESIZE,
        params_schema: schema_of::<AgentResizeParams>,
        result_schema: schema_of::<AgentResizeResult>,
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
];

/// One server → client push's typed metadata.
pub struct PushSpec {
    /// The wire method name.
    pub method: &'static str,
    /// Returns the JSON Schema of the params (push payload) type.
    pub params_schema: fn() -> serde_json::Value,
}

/// Every server → client push with its payload schema, in the same order as
/// [`SERVER_PUSHES`](super::SERVER_PUSHES).
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
        method: super::push::AGENT_OUTPUT,
        params_schema: schema_of::<AgentOutputPush>,
    },
    PushSpec {
        method: super::push::AGENT_EXIT,
        params_schema: schema_of::<AgentExitPush>,
    },
    PushSpec {
        method: super::push::AGENT_STATE,
        params_schema: schema_of::<AgentStatePush>,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

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
            needs: None,
            vars: Vec::new(),
            prompt: "p".to_string(),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["agent"], serde_json::Value::Null);
        assert_eq!(json["prompt"], "p");

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
}
