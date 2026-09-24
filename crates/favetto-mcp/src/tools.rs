//! The closed supervisor tool set and its mapping onto daemon RPCs.
//!
//! Every tool is a thin, typed projection of one allowed RPC (or, for
//! `favetto_wait`, a bounded local wait on the event cursor). There is no generic
//! passthrough: mutations are limited to the four actions the
//! [supervisor contract](../../../docs/reference/supervisor-contract.md) defines
//! (`spawn`, `cancel`, `retry` plus the read-only observation tools), and the
//! only input path is `agents.reply`. Raw PTY writes (`agents.input`) are never
//! exposed.
//!
//! [`resolve`] is a pure function from `(name, args)` to a [`ToolCall`], so the
//! mapping can be unit-tested without a daemon.

use serde_json::{json, Map, Value};

use favetto_core::rpc::method;

/// Default bound for `favetto_wait`, in milliseconds.
pub const DEFAULT_WAIT_MS: u64 = 30_000;
/// Hard cap for `favetto_wait`, so a tool call can never block indefinitely.
pub const MAX_WAIT_MS: u64 = 120_000;

/// A declared tool: its name, human/LLM-facing description, and JSON Schema.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

/// How a resolved tool call is dispatched.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolCall {
    /// Forward `params` to the named daemon RPC.
    Rpc { method: &'static str, params: Value },
    /// Wait locally on the event cursor.
    Wait(WaitArgs),
}

/// Bounded wait parameters for `favetto_wait`.
#[derive(Debug, Clone, PartialEq)]
pub struct WaitArgs {
    /// Event kinds that should wake the wait early. Empty means "any event".
    pub until: Vec<String>,
    /// Maximum time to wait, clamped to [`MAX_WAIT_MS`].
    pub timeout_ms: u64,
    /// Accepted for symmetry with the contract; currently advisory.
    pub root_id: Option<String>,
}

/// A validation/dispatch failure, surfaced to the MCP client as a tool
/// execution error (`isError: true`), never a protocol error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError(pub String);

impl ToolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ToolError {}

/// Every tool in the closed vocabulary.
pub fn list() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "favetto_inspect",
            description: "Observe a workflow root: its aggregate state and one entry per \
                          task instance (id, name, status, attempt, summary). Read-only.",
            input_schema: object(
                json!({
                    "root_id": {"type": "string", "description": "Workflow root task id (uuid)."}
                }),
                &["root_id"],
            ),
        },
        ToolSpec {
            name: "favetto_start_task",
            description: "Start a catalog task by name as a brand-new workflow root and return \
                          its task row (whose `id` is the root id). Always headless.",
            input_schema: object(
                json!({
                    "name": {"type": "string", "description": "Catalog task name."},
                    "input": {"description": "Optional JSON input passed to the task."}
                }),
                &["name"],
            ),
        },
        ToolSpec {
            name: "favetto_spawn",
            description: "Add one task to an existing workflow root, optionally depending on \
                          existing task ids. The controller owns the sequencing; the daemon \
                          stores the runtime edge.",
            input_schema: object(
                json!({
                    "root_id": {"type": "string", "description": "Existing root id (uuid)."},
                    "name": {"type": "string", "description": "Catalog task name."},
                    "input": {"description": "Optional JSON input passed to the task."},
                    "depends_on": {
                        "type": "array", "items": {"type": "string"},
                        "description": "Task instance ids (uuid) this task depends on."
                    },
                    "dedupe_key": {
                        "type": "string",
                        "description": "Idempotency key; a re-issued spawn with the same key is a no-op."
                    }
                }),
                &["name"],
            ),
        },
        ToolSpec {
            name: "favetto_create_workflow",
            description: "Create a runtime DAG of catalog tasks with per-instance dependencies \
                          in one call, idempotent on `idempotency_key`.",
            input_schema: object(
                json!({
                    "idempotency_key": {"type": "string", "description": "Scopes the per-node dedupe keys."},
                    "root_id": {"type": "string", "description": "Optionally extend an existing root."},
                    "tasks": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "key": {"type": "string", "description": "Request-local node key."},
                                "name": {"type": "string", "description": "Catalog task name."},
                                "input": {"description": "Optional JSON input."},
                                "depends_on": {
                                    "type": "array", "items": {"type": "string"},
                                    "description": "Sibling node keys this task depends on."
                                }
                            },
                            "required": ["key", "name"],
                            "additionalProperties": false
                        }
                    }
                }),
                &["idempotency_key", "tasks"],
            ),
        },
        ToolSpec {
            name: "favetto_cancel_workflow",
            description: "Cancel every non-terminal task in a workflow root. Already-terminal \
                          tasks are left untouched.",
            input_schema: object(
                json!({
                    "root_id": {"type": "string", "description": "Workflow root task id (uuid)."}
                }),
                &["root_id"],
            ),
        },
        ToolSpec {
            name: "favetto_cancel_task",
            description: "Cancel a single task instance by id.",
            input_schema: object(
                json!({
                    "id": {"type": "string", "description": "Task instance id (uuid)."}
                }),
                &["id"],
            ),
        },
        ToolSpec {
            name: "favetto_retry_task",
            description: "Retry a terminal task, preserving its prior run history. This is the \
                          contract's primary retry (`workflow.retry`).",
            input_schema: object(
                json!({
                    "task_id": {"type": "string", "description": "Terminal task instance id (uuid)."}
                }),
                &["task_id"],
            ),
        },
        ToolSpec {
            name: "favetto_get_task",
            description: "Fetch one task instance by id, including its stored output.",
            input_schema: object(
                json!({
                    "id": {"type": "string", "description": "Task instance id (uuid)."}
                }),
                &["id"],
            ),
        },
        ToolSpec {
            name: "favetto_list_tasks",
            description: "List recent tasks (metadata only; output is omitted).",
            input_schema: object(
                json!({
                    "limit": {"type": "integer", "minimum": 0, "description": "Maximum rows (capped at 2000)."}
                }),
                &[],
            ),
        },
        ToolSpec {
            name: "favetto_agents_list",
            description: "List configured agents and live sessions, including each session's \
                          `awaiting_input` reason. Read-only.",
            input_schema: object(json!({}), &[]),
        },
        ToolSpec {
            name: "favetto_agents_reply",
            description: "Answer a structured input request on a session. This is the ONLY agent \
                          input path exposed: there is no raw PTY write.",
            input_schema: object(
                json!({
                    "session_id": {"type": "string", "description": "Agent session id."},
                    "request_id": {"type": "string", "description": "Input request id from agents.list."},
                    "reply": {
                        "type": "string",
                        "enum": ["once", "always", "reject", "value", "confirmed", "cancelled"],
                        "description": "Reply kind."
                    },
                    "value": {"type": "string", "description": "Required when reply is `value`."},
                    "confirmed": {"type": "boolean", "description": "Required when reply is `confirmed`."}
                }),
                &["session_id", "request_id", "reply"],
            ),
        },
        ToolSpec {
            name: "favetto_events_tail",
            description: "Tail persisted events without subscribing.",
            input_schema: object(
                json!({
                    "limit": {"type": "integer", "minimum": 0, "description": "Maximum events (capped at 1000)."}
                }),
                &[],
            ),
        },
        ToolSpec {
            name: "favetto_wait",
            description: "Block on the event cursor until a matching event arrives or the timeout \
                          elapses. Mutates nothing; returns the observed events, the advanced \
                          cursor, and whether it timed out.",
            input_schema: object(
                json!({
                    "until": {
                        "type": "array", "items": {"type": "string"},
                        "description": "Event kinds that wake the wait early (e.g. [\"task_finished\"]). Empty = any event."
                    },
                    "timeout_ms": {
                        "type": "integer", "minimum": 0, "maximum": MAX_WAIT_MS,
                        "description": "Maximum time to wait in milliseconds. Defaults to 30000, capped at 120000."
                    },
                    "root_id": {"type": "string", "description": "Optional root id for context."}
                }),
                &[],
            ),
        },
    ]
}

/// The names deliberately excluded from the vocabulary. Asserted absent by tests
/// so a future change cannot silently widen the surface.
pub const EXCLUDED: &[&str] = &[
    method::AGENTS_INPUT,
    method::AGENTS_START,
    method::AGENTS_RESIZE,
    method::AGENTS_ATTACH,
    method::AGENTS_CLOSE,
    method::TASKS_START_ONESHOT,
    method::CATALOG_ADD,
    method::CATALOG_UPDATE,
    method::SCHEDULES_LIST,
    method::SCHEDULES_UPSERT,
    method::SCHEDULES_DELETE,
    method::NOTIFICATIONS_LIST,
    method::NOTIFICATIONS_TEST,
    method::HOOKS_UPSERT,
    method::PING,
];

/// Build a JSON Schema object with `additionalProperties: false`.
fn object(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

/// Map a tool name plus arguments to the RPC (or local wait) it performs.
pub fn resolve(name: &str, args: &Value) -> Result<ToolCall, ToolError> {
    match name {
        "favetto_inspect" => {
            let root_id = require_str(args, "root_id")?;
            Ok(rpc(method::WORKFLOW_INSPECT, json!({ "root_id": root_id })))
        }
        "favetto_start_task" => {
            let name = require_str(args, "name")?;
            let mut params = Map::new();
            params.insert("name".to_string(), json!(name));
            copy_optional(args, "input", &mut params);
            Ok(rpc(method::TASKS_START, Value::Object(params)))
        }
        "favetto_spawn" => {
            let name = require_str(args, "name")?;
            let mut params = Map::new();
            params.insert("name".to_string(), json!(name));
            for key in ["root_id", "input", "depends_on", "dedupe_key"] {
                copy_optional(args, key, &mut params);
            }
            Ok(rpc(method::WORKFLOW_SPAWN, Value::Object(params)))
        }
        "favetto_create_workflow" => {
            let idempotency_key = require_str(args, "idempotency_key")?;
            let tasks = args
                .get("tasks")
                .and_then(Value::as_array)
                .ok_or_else(|| ToolError::new("missing required array parameter `tasks`"))?
                .iter()
                .map(validate_create_task)
                .collect::<Result<Vec<_>, _>>()?;
            let mut params = Map::new();
            params.insert("idempotency_key".to_string(), json!(idempotency_key));
            params.insert("tasks".to_string(), Value::Array(tasks));
            copy_optional(args, "root_id", &mut params);
            Ok(rpc(method::WORKFLOW_CREATE, Value::Object(params)))
        }
        "favetto_cancel_workflow" => {
            let root_id = require_str(args, "root_id")?;
            Ok(rpc(method::WORKFLOW_CANCEL, json!({ "root_id": root_id })))
        }
        "favetto_cancel_task" => {
            let id = require_str(args, "id")?;
            Ok(rpc(method::TASKS_CANCEL, json!({ "id": id })))
        }
        "favetto_retry_task" => {
            let task_id = require_str(args, "task_id")?;
            Ok(rpc(method::WORKFLOW_RETRY, json!({ "task_id": task_id })))
        }
        "favetto_get_task" => {
            let id = require_str(args, "id")?;
            Ok(rpc(method::TASKS_GET, json!({ "id": id })))
        }
        "favetto_list_tasks" => {
            let mut params = Map::new();
            copy_optional_number(args, "limit", &mut params)?;
            Ok(rpc(method::TASKS_LIST, Value::Object(params)))
        }
        "favetto_agents_list" => Ok(rpc(method::AGENTS_LIST, json!({}))),
        "favetto_agents_reply" => {
            let session_id = require_str(args, "session_id")?;
            let request_id = require_str(args, "request_id")?;
            let reply = reply_value(args)?;
            Ok(rpc(
                method::AGENTS_REPLY,
                json!({
                    "session_id": session_id,
                    "request_id": request_id,
                    "reply": reply,
                }),
            ))
        }
        "favetto_events_tail" => {
            let mut params = Map::new();
            copy_optional_number(args, "limit", &mut params)?;
            Ok(rpc(method::EVENTS_TAIL, Value::Object(params)))
        }
        "favetto_wait" => Ok(ToolCall::Wait(wait_args(args)?)),
        other => Err(ToolError::new(format!("unknown tool `{other}`"))),
    }
}

fn rpc(method: &'static str, params: Value) -> ToolCall {
    ToolCall::Rpc { method, params }
}

/// Copy `args[key]` into `params` when present and not JSON `null`.
fn copy_optional(args: &Value, key: &str, params: &mut Map<String, Value>) {
    if let Some(value) = args.get(key) {
        if !value.is_null() {
            params.insert(key.to_string(), value.clone());
        }
    }
}

/// Copy an optional numeric argument, rejecting a non-integer value.
fn copy_optional_number(
    args: &Value,
    key: &str,
    params: &mut Map<String, Value>,
) -> Result<(), ToolError> {
    if let Some(value) = args.get(key) {
        if !value.is_null() {
            if !value.is_u64() {
                return Err(ToolError::new(format!(
                    "parameter `{key}` must be a non-negative integer"
                )));
            }
            params.insert(key.to_string(), value.clone());
        }
    }
    Ok(())
}

fn require_str(args: &Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ToolError::new(format!("missing required string parameter `{key}`")))
}

/// Map the flat `reply` vocabulary onto the tagged `InputReply` wire shape.
fn reply_value(args: &Value) -> Result<Value, ToolError> {
    let reply = require_str(args, "reply")?;
    match reply.as_str() {
        "once" => Ok(json!({ "reply": "once" })),
        "always" => Ok(json!({ "reply": "always" })),
        "reject" => Ok(json!({ "reply": "reject" })),
        "cancelled" => Ok(json!({ "reply": "cancelled" })),
        "value" => {
            let value = args
                .get("value")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::new("reply `value` requires a string `value`"))?;
            Ok(json!({ "reply": "value", "value": value }))
        }
        "confirmed" => {
            let confirmed = args
                .get("confirmed")
                .and_then(Value::as_bool)
                .ok_or_else(|| {
                    ToolError::new("reply `confirmed` requires a boolean `confirmed`")
                })?;
            Ok(json!({ "reply": "confirmed", "confirmed": confirmed }))
        }
        other => Err(ToolError::new(format!("unknown reply kind `{other}`"))),
    }
}

fn wait_args(args: &Value) -> Result<WaitArgs, ToolError> {
    let until = match args.get("until") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| ToolError::new("`until` must be an array of strings"))
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err(ToolError::new("`until` must be an array of strings")),
    };
    let timeout_ms = match args.get("timeout_ms") {
        None | Some(Value::Null) => DEFAULT_WAIT_MS,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| ToolError::new("`timeout_ms` must be a non-negative integer"))?
            .min(MAX_WAIT_MS),
    };
    let root_id = args
        .get("root_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(WaitArgs {
        until,
        timeout_ms,
        root_id,
    })
}

/// Validate one `workflow.create` node, rebuilding it to the daemon's shape.
fn validate_create_task(value: &Value) -> Result<Value, ToolError> {
    let object = value
        .as_object()
        .ok_or_else(|| ToolError::new("each `tasks` entry must be an object"))?;
    let key = object
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::new("each `tasks` entry requires a string `key`"))?;
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::new("each `tasks` entry requires a string `name`"))?;
    let mut out = Map::new();
    out.insert("key".to_string(), json!(key));
    out.insert("name".to_string(), json!(name));
    if let Some(input) = object.get("input") {
        if !input.is_null() {
            out.insert("input".to_string(), input.clone());
        }
    }
    if let Some(depends_on) = object.get("depends_on") {
        if !depends_on.is_null() {
            let deps = depends_on.as_array().ok_or_else(|| {
                ToolError::new("each `tasks[].depends_on` must be an array of strings")
            })?;
            if !deps.iter().all(Value::is_string) {
                return Err(ToolError::new(
                    "each `tasks[].depends_on` must be an array of strings",
                ));
            }
            out.insert("depends_on".to_string(), depends_on.clone());
        }
    }
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve_method(name: &str, args: &Value) -> (&'static str, Value) {
        match resolve(name, args).unwrap_or_else(|e| panic!("{name}: {e}")) {
            ToolCall::Rpc { method, params } => (method, params),
            other => panic!("{name} resolved to {other:?}"),
        }
    }

    #[test]
    fn names_are_unique_and_described() {
        let tools = list();
        assert_eq!(tools.len(), 13, "the vocabulary is closed at 13 tools");
        let mut names: Vec<&str> = tools.iter().map(|t| t.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), tools.len(), "tool names must be unique");
        for tool in &tools {
            assert!(!tool.description.trim().is_empty(), "{}", tool.name);
            assert!(
                tool.input_schema.is_object(),
                "{} needs an object schema",
                tool.name
            );
        }
    }

    #[test]
    fn every_tool_maps_to_the_expected_rpc() {
        let uuid = "00000000-0000-0000-0000-000000000001";
        let cases: Vec<(&str, Value, &str, Value)> = vec![
            (
                "favetto_inspect",
                json!({"root_id": uuid}),
                method::WORKFLOW_INSPECT,
                json!({"root_id": uuid}),
            ),
            (
                "favetto_start_task",
                json!({"name": "plan", "input": 3}),
                method::TASKS_START,
                json!({"name": "plan", "input": 3}),
            ),
            (
                "favetto_spawn",
                json!({"name": "impl", "root_id": uuid, "dedupe_key": "d"}),
                method::WORKFLOW_SPAWN,
                json!({"name": "impl", "root_id": uuid, "dedupe_key": "d"}),
            ),
            (
                "favetto_create_workflow",
                json!({"idempotency_key": "k", "tasks": [{"key": "a", "name": "plan"}]}),
                method::WORKFLOW_CREATE,
                json!({"idempotency_key": "k", "tasks": [{"key": "a", "name": "plan"}]}),
            ),
            (
                "favetto_cancel_workflow",
                json!({"root_id": uuid}),
                method::WORKFLOW_CANCEL,
                json!({"root_id": uuid}),
            ),
            (
                "favetto_cancel_task",
                json!({"id": uuid}),
                method::TASKS_CANCEL,
                json!({"id": uuid}),
            ),
            (
                "favetto_retry_task",
                json!({"task_id": uuid}),
                method::WORKFLOW_RETRY,
                json!({"task_id": uuid}),
            ),
            (
                "favetto_get_task",
                json!({"id": uuid}),
                method::TASKS_GET,
                json!({"id": uuid}),
            ),
            (
                "favetto_list_tasks",
                json!({"limit": 10}),
                method::TASKS_LIST,
                json!({"limit": 10}),
            ),
            (
                "favetto_agents_list",
                json!({}),
                method::AGENTS_LIST,
                json!({}),
            ),
            (
                "favetto_events_tail",
                json!({"limit": 5}),
                method::EVENTS_TAIL,
                json!({"limit": 5}),
            ),
            (
                "favetto_agents_reply",
                json!({"session_id": "s", "request_id": "r", "reply": "once"}),
                method::AGENTS_REPLY,
                json!({"session_id": "s", "request_id": "r", "reply": {"reply": "once"}}),
            ),
        ];
        for (name, args, expected_method, expected_params) in cases {
            let (m, p) = resolve_method(name, &args);
            assert_eq!(m, expected_method, "{name}");
            assert_eq!(p, expected_params, "{name}");
        }
    }

    #[test]
    fn optional_params_are_omitted_when_absent() {
        let (_, params) = resolve_method("favetto_list_tasks", &json!({}));
        assert_eq!(params, json!({}));
        let (_, params) = resolve_method("favetto_start_task", &json!({"name": "x"}));
        assert_eq!(params, json!({"name": "x"}));
    }

    #[test]
    fn agents_reply_builds_each_tagged_shape() {
        let base = json!({"session_id": "s", "request_id": "r"});
        let cases = [
            (json!({"reply": "once"}), json!({"reply": "once"})),
            (json!({"reply": "always"}), json!({"reply": "always"})),
            (json!({"reply": "reject"}), json!({"reply": "reject"})),
            (json!({"reply": "cancelled"}), json!({"reply": "cancelled"})),
            (
                json!({"reply": "value", "value": "hunter2"}),
                json!({"reply": "value", "value": "hunter2"}),
            ),
            (
                json!({"reply": "confirmed", "confirmed": true}),
                json!({"reply": "confirmed", "confirmed": true}),
            ),
        ];
        for (extra, expected_reply) in cases {
            let mut args = base.clone();
            for (k, v) in extra.as_object().unwrap() {
                args[k] = v.clone();
            }
            let (m, params) = resolve_method("favetto_agents_reply", &args);
            assert_eq!(m, method::AGENTS_REPLY);
            assert_eq!(params["reply"], expected_reply, "{extra}");
        }
    }

    #[test]
    fn agents_reply_rejects_missing_value_or_confirmation() {
        let value = resolve(
            "favetto_agents_reply",
            &json!({"session_id": "s", "request_id": "r", "reply": "value"}),
        );
        assert!(value.is_err());
        let confirmed = resolve(
            "favetto_agents_reply",
            &json!({"session_id": "s", "request_id": "r", "reply": "confirmed"}),
        );
        assert!(confirmed.is_err());
        let unknown = resolve(
            "favetto_agents_reply",
            &json!({"session_id": "s", "request_id": "r", "reply": "maybe"}),
        );
        assert!(unknown.is_err());
    }

    #[test]
    fn missing_required_arguments_are_rejected() {
        assert!(resolve("favetto_inspect", &json!({})).is_err());
        assert!(resolve("favetto_spawn", &json!({})).is_err());
        assert!(resolve("favetto_get_task", &json!({})).is_err());
        assert!(resolve("favetto_create_workflow", &json!({"idempotency_key": "k"})).is_err());
    }

    #[test]
    fn wait_clamps_timeout_and_parses_until() {
        match resolve(
            "favetto_wait",
            &json!({"until": ["task_finished"], "timeout_ms": 999_999, "root_id": "r"}),
        )
        .unwrap()
        {
            ToolCall::Wait(args) => {
                assert_eq!(args.until, vec!["task_finished".to_string()]);
                assert_eq!(args.timeout_ms, MAX_WAIT_MS);
                assert_eq!(args.root_id.as_deref(), Some("r"));
            }
            other => panic!("expected wait, got {other:?}"),
        }
        match resolve("favetto_wait", &json!({})).unwrap() {
            ToolCall::Wait(args) => {
                assert!(args.until.is_empty());
                assert_eq!(args.timeout_ms, DEFAULT_WAIT_MS);
            }
            other => panic!("expected wait, got {other:?}"),
        }
        assert!(resolve("favetto_wait", &json!({"until": "task_finished"})).is_err());
    }

    #[test]
    fn excluded_methods_are_absent_from_the_vocabulary() {
        // The exposed tool names are all `favetto_*`; the guard is that no tool
        // resolves to an excluded RPC. Enumerate the mapping with the same args
        // the mapping test uses and assert the target methods stay in bounds.
        let uuid = "00000000-0000-0000-0000-000000000001";
        let calls: Vec<(&str, Value)> = vec![
            ("favetto_inspect", json!({"root_id": uuid})),
            ("favetto_start_task", json!({"name": "x"})),
            ("favetto_spawn", json!({"name": "x"})),
            (
                "favetto_create_workflow",
                json!({"idempotency_key": "k", "tasks": [{"key": "a", "name": "x"}]}),
            ),
            ("favetto_cancel_workflow", json!({"root_id": uuid})),
            ("favetto_cancel_task", json!({"id": uuid})),
            ("favetto_retry_task", json!({"task_id": uuid})),
            ("favetto_get_task", json!({"id": uuid})),
            ("favetto_list_tasks", json!({})),
            ("favetto_agents_list", json!({})),
            (
                "favetto_agents_reply",
                json!({"session_id": "s", "request_id": "r", "reply": "once"}),
            ),
            ("favetto_events_tail", json!({})),
        ];
        for (name, args) in calls {
            if let ToolCall::Rpc { method, .. } = resolve(name, &args).unwrap() {
                assert!(
                    !EXCLUDED.contains(&method),
                    "`{name}` exposes excluded method `{method}`"
                );
            }
        }
        assert!(resolve("agents.input", &json!({})).is_err());
        assert!(resolve("system.ping", &json!({})).is_err());
    }
}
