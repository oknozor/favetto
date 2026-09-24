# Remote API

Generated from `crates/favetto-core/src/rpc.rs` by the docs generator. Do not edit by hand.

The daemon exposes one wire protocol over two transports:

- **Unix socket** `/tmp/favetto.sock` (local, trusted).
- **WebSocket** `ws://127.0.0.1:7878/rpc` (bearer token required).

Both carry the same MessagePack-encoded frames. A frame is a tagged envelope with one of three shapes:

```rust
enum Frame {
    Request(Request),           // client → server
    Response(Response),         // server → client reply
    Notification(Notification), // unsolicited server push
}
```

A `Request` carries `id`, `method`, and `params`; the matching `Response` carries the same `id` plus either `result` or a structured `error` (`code`, `message`, optional `data`).

## Client → server methods

| Method | Purpose |
|--------|---------|
| `system.ping` | Liveness check. |
| `tasks.list` | List tasks (metadata only; output is omitted). |
| `tasks.get` | Fetch a single task by id, including its stored output. |
| `tasks.cancel` | Cancel a task. |
| `tasks.retry` | Retry a terminal task, preserving its prior run history. |
| `tasks.start` | Start a catalog task by name. |
| `tasks.start_oneshot` | Start a one-shot task from an inline definition (not added to the catalog) and open its interactive agent session. |
| `catalog.list` | List the task catalog. |
| `catalog.get` | Fetch a catalog task's raw Markdown source (for the preview pane). |
| `catalog.add` | Add a task definition to the catalog (does not run it). |
| `catalog.update` | Replace an existing catalog task's raw Markdown source (does not run it). |
| `workflow.get` | Fetch the catalog workflow graph as Graphviz DOT plus a structured graph. |
| `workflow.inspect` | Fetch the runtime workflow graph for a root (task instances plus ready/running/failed/blocked buckets). |
| `workflow.create` | Create a runtime DAG of catalog tasks with per-instance dependencies. Idempotent on `idempotency_key`. |
| `workflow.spawn` | Add one runtime task to an existing workflow root, optionally depending on existing task ids. |
| `workflow.cancel` | Cancel every non-terminal task in a workflow root, emitting `TaskCancelled` per task. |
| `workflow.retry` | Retry a terminal task, preserving its prior run history. Single-task alias of `tasks.retry`. |
| `events.tail` | Tail persisted events. |
| `events.subscribe` | (Re)subscribe to the live event stream; accepts `last_event_id` to replay missed events before switching to live delivery. |
| `agents.list` | List configured external agents and live agent sessions, including each agent's `available` flag and capability flags. |
| `agents.start` | Start an external agent session (optionally attached to a task). |
| `agents.input` | Write raw bytes (base64) to a session's PTY. |
| `agents.resize` | Resize a session's PTY. |
| `agents.attach` | Attach to a session: returns the session and a replay of its output. |
| `agents.close` | Terminate a session. |
| `agents.reply` | Answer a structured input request on a session's state channel. |
| `providers.list` | List configured providers and their available models. |
| `schedules.list` | List cron schedules. |
| `schedules.upsert` | Create or update a cron schedule. |
| `schedules.delete` | Delete a cron schedule. |
| `notifications.list` | List recent notifications. |
| `notifications.test` | Send a test notification through a channel. |
| `hooks.upsert` | Add a notification hook reacting to an event kind. |
| `usage.stats` | Aggregate persisted per-run token/cost usage into a per-bucket series (day/week/month/year) plus window totals. |

## Client → server request & response types

Every method's `params` and `result` are typed in `favetto-core::rpc::messages`. The tables below are generated from those types' JSON Schemas.

### `system.ping`

Liveness check.

**Request**

object

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `cwd` | string | — | yes | The daemon process's current working directory. |
| `pong` | boolean | — | yes | Always `true` for a live daemon. |

### `tasks.list`

List tasks (metadata only; output is omitted).

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `limit` | integer (optional) | null | no |  |

**Response**

Task[]

### `tasks.get`

Fetch a single task by id, including its stored output.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `id` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `attempt` | integer | `0` | no | How many execution attempts this task has had. Starts at 0; each claim increments it and records a [`TaskRun`] under the new attempt. Defaults to 0 so rows and payloads written before attempts existed decode unchanged. |
| `created_at` | string | — | yes |  |
| `dedupe_key` | string (optional) | — | no | Optional dedupe key. Inserting a second task with the same key is a no-op. |
| `error` | string (optional) | — | no | Human-readable failure reason when `status == Failed`. |
| `failure` | Failure (optional) | — | no | Machine-readable failure classification when `status == Failed`. Absent on success and on rows/payloads written before typed failures existed. |
| `finished_at` | string (optional) | — | no |  |
| `id` | string | — | yes |  |
| `input` | any | — | yes | Arbitrary input passed to the task's prompt. |
| `interactive` | boolean | `false` | no | True when the run was started by a user and should execute in the agent's interactive TUI (the Agent panel attaches to it live). Programmatic starts (scheduler, webhooks, hooks, `needs`, `spawn`) leave this false and run headless. Defaults to false so older payloads and rows decode unchanged. |
| `name` | string | — | yes | Name of the catalog task (the `*.md` file) that defines how to run this. |
| `output` | any | — | no | Produced by the task on completion. Omitted from list and push payloads; fetch it on demand with `tasks.get`. |
| `parent_id` | string (optional) | — | no | The task that directly enqueued this one (a `spawn` parent, a `needs` predecessor, …). `None` for a task started directly (manual, scheduled, RPC, hook, webhook). |
| `root_id` | string (optional) | — | no | The workflow origin this run belongs to. A directly-started task has no root of its own (it *is* the root, so [`Task::root_or_self`] falls back to its id); a spawned child inherits its parent's root. |
| `session_id` | string (optional) | — | no | The agent's own session id (e.g. opencode's session id), captured from a task run's output so the session can be reattached later. |
| `session_title` | string (optional) | — | no | The agent's human-readable session title (e.g. opencode's generated title), resolved once at run completion from the agent's session store. Absent until a run finishes or when the agent reports no title. |
| `started_at` | string (optional) | — | no |  |
| `status` | TaskStatus | — | yes |  |

### `tasks.cancel`

Cancel a task.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `id` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `attempt` | integer | `0` | no | How many execution attempts this task has had. Starts at 0; each claim increments it and records a [`TaskRun`] under the new attempt. Defaults to 0 so rows and payloads written before attempts existed decode unchanged. |
| `created_at` | string | — | yes |  |
| `dedupe_key` | string (optional) | — | no | Optional dedupe key. Inserting a second task with the same key is a no-op. |
| `error` | string (optional) | — | no | Human-readable failure reason when `status == Failed`. |
| `failure` | Failure (optional) | — | no | Machine-readable failure classification when `status == Failed`. Absent on success and on rows/payloads written before typed failures existed. |
| `finished_at` | string (optional) | — | no |  |
| `id` | string | — | yes |  |
| `input` | any | — | yes | Arbitrary input passed to the task's prompt. |
| `interactive` | boolean | `false` | no | True when the run was started by a user and should execute in the agent's interactive TUI (the Agent panel attaches to it live). Programmatic starts (scheduler, webhooks, hooks, `needs`, `spawn`) leave this false and run headless. Defaults to false so older payloads and rows decode unchanged. |
| `name` | string | — | yes | Name of the catalog task (the `*.md` file) that defines how to run this. |
| `output` | any | — | no | Produced by the task on completion. Omitted from list and push payloads; fetch it on demand with `tasks.get`. |
| `parent_id` | string (optional) | — | no | The task that directly enqueued this one (a `spawn` parent, a `needs` predecessor, …). `None` for a task started directly (manual, scheduled, RPC, hook, webhook). |
| `root_id` | string (optional) | — | no | The workflow origin this run belongs to. A directly-started task has no root of its own (it *is* the root, so [`Task::root_or_self`] falls back to its id); a spawned child inherits its parent's root. |
| `session_id` | string (optional) | — | no | The agent's own session id (e.g. opencode's session id), captured from a task run's output so the session can be reattached later. |
| `session_title` | string (optional) | — | no | The agent's human-readable session title (e.g. opencode's generated title), resolved once at run completion from the agent's session store. Absent until a run finishes or when the agent reports no title. |
| `started_at` | string (optional) | — | no |  |
| `status` | TaskStatus | — | yes |  |

### `tasks.retry`

Retry a terminal task, preserving its prior run history.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `id` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `attempt` | integer | `0` | no | How many execution attempts this task has had. Starts at 0; each claim increments it and records a [`TaskRun`] under the new attempt. Defaults to 0 so rows and payloads written before attempts existed decode unchanged. |
| `created_at` | string | — | yes |  |
| `dedupe_key` | string (optional) | — | no | Optional dedupe key. Inserting a second task with the same key is a no-op. |
| `error` | string (optional) | — | no | Human-readable failure reason when `status == Failed`. |
| `failure` | Failure (optional) | — | no | Machine-readable failure classification when `status == Failed`. Absent on success and on rows/payloads written before typed failures existed. |
| `finished_at` | string (optional) | — | no |  |
| `id` | string | — | yes |  |
| `input` | any | — | yes | Arbitrary input passed to the task's prompt. |
| `interactive` | boolean | `false` | no | True when the run was started by a user and should execute in the agent's interactive TUI (the Agent panel attaches to it live). Programmatic starts (scheduler, webhooks, hooks, `needs`, `spawn`) leave this false and run headless. Defaults to false so older payloads and rows decode unchanged. |
| `name` | string | — | yes | Name of the catalog task (the `*.md` file) that defines how to run this. |
| `output` | any | — | no | Produced by the task on completion. Omitted from list and push payloads; fetch it on demand with `tasks.get`. |
| `parent_id` | string (optional) | — | no | The task that directly enqueued this one (a `spawn` parent, a `needs` predecessor, …). `None` for a task started directly (manual, scheduled, RPC, hook, webhook). |
| `root_id` | string (optional) | — | no | The workflow origin this run belongs to. A directly-started task has no root of its own (it *is* the root, so [`Task::root_or_self`] falls back to its id); a spawned child inherits its parent's root. |
| `session_id` | string (optional) | — | no | The agent's own session id (e.g. opencode's session id), captured from a task run's output so the session can be reattached later. |
| `session_title` | string (optional) | — | no | The agent's human-readable session title (e.g. opencode's generated title), resolved once at run completion from the agent's session store. Absent until a run finishes or when the agent reports no title. |
| `started_at` | string (optional) | — | no |  |
| `status` | TaskStatus | — | yes |  |

### `tasks.start`

Start a catalog task by name.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `input` | any | null | no |  |
| `interactive` | boolean (optional) | null | no |  |
| `name` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `attempt` | integer | `0` | no | How many execution attempts this task has had. Starts at 0; each claim increments it and records a [`TaskRun`] under the new attempt. Defaults to 0 so rows and payloads written before attempts existed decode unchanged. |
| `created_at` | string | — | yes |  |
| `dedupe_key` | string (optional) | — | no | Optional dedupe key. Inserting a second task with the same key is a no-op. |
| `error` | string (optional) | — | no | Human-readable failure reason when `status == Failed`. |
| `failure` | Failure (optional) | — | no | Machine-readable failure classification when `status == Failed`. Absent on success and on rows/payloads written before typed failures existed. |
| `finished_at` | string (optional) | — | no |  |
| `id` | string | — | yes |  |
| `input` | any | — | yes | Arbitrary input passed to the task's prompt. |
| `interactive` | boolean | `false` | no | True when the run was started by a user and should execute in the agent's interactive TUI (the Agent panel attaches to it live). Programmatic starts (scheduler, webhooks, hooks, `needs`, `spawn`) leave this false and run headless. Defaults to false so older payloads and rows decode unchanged. |
| `name` | string | — | yes | Name of the catalog task (the `*.md` file) that defines how to run this. |
| `output` | any | — | no | Produced by the task on completion. Omitted from list and push payloads; fetch it on demand with `tasks.get`. |
| `parent_id` | string (optional) | — | no | The task that directly enqueued this one (a `spawn` parent, a `needs` predecessor, …). `None` for a task started directly (manual, scheduled, RPC, hook, webhook). |
| `root_id` | string (optional) | — | no | The workflow origin this run belongs to. A directly-started task has no root of its own (it *is* the root, so [`Task::root_or_self`] falls back to its id); a spawned child inherits its parent's root. |
| `session_id` | string (optional) | — | no | The agent's own session id (e.g. opencode's session id), captured from a task run's output so the session can be reattached later. |
| `session_title` | string (optional) | — | no | The agent's human-readable session title (e.g. opencode's generated title), resolved once at run completion from the agent's session store. Absent until a run finishes or when the agent reports no title. |
| `started_at` | string (optional) | — | no |  |
| `status` | TaskStatus | — | yes |  |

### `tasks.start_oneshot`

Start a one-shot task from an inline definition (not added to the catalog) and open its interactive agent session.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `agent` | string (optional) | null | no |  |
| `cols` | integer (optional) | null | no |  |
| `cwd` | string (optional) | null | no |  |
| `model` | string (optional) | null | no |  |
| `provider` | string (optional) | null | no |  |
| `rows` | integer (optional) | null | no |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `data` | string | — | yes | Base64-encoded `vt100` screen frame. |
| `session` | AgentSessionInfo | — | yes |  |
| `task` | Task | — | yes |  |

### `catalog.list`

List the task catalog.

**Request**

object

**Response**

CatalogEntry[]

### `catalog.get`

Fetch a catalog task's raw Markdown source (for the preview pane).

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `name` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `markdown` | string | — | yes |  |

### `catalog.add`

Add a task definition to the catalog (does not run it).

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `agent` | string (optional) | null | no |  |
| `cwd` | string (optional) | null | no |  |
| `model` | string (optional) | null | no |  |
| `name` | string | — | yes |  |
| `needs` | string (optional) | null | no |  |
| `prompt` | string (optional) | null | no |  |
| `provider` | string (optional) | null | no |  |
| `schedule` | string (optional) | null | no |  |
| `spawn` | string (optional) | null | no |  |
| `spawn_file` | string (optional) | null | no |  |
| `spawn_new_root` | boolean (optional) | null | no |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `agent` | string (optional) | — | no |  |
| `cwd` | string (optional) | — | no |  |
| `model` | string (optional) | — | no |  |
| `name` | string | — | yes |  |
| `needs` | string (optional) | — | no |  |
| `provider` | string (optional) | — | no |  |
| `schedule` | string (optional) | — | no |  |
| `vars` | TaskVar[] | — | yes |  |

### `catalog.update`

Replace an existing catalog task's raw Markdown source (does not run it).

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `markdown` | string | — | yes |  |
| `name` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `updated` | boolean | — | yes |  |

### `workflow.get`

Fetch the catalog workflow graph as Graphviz DOT plus a structured graph.

**Request**

object

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `dot` | string | — | yes |  |
| `graph` | WorkflowGraph | — | yes |  |
| `path` | string | — | yes |  |

### `workflow.inspect`

Fetch the runtime workflow graph for a root (task instances plus ready/running/failed/blocked buckets).

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `root_id` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `blocked` | string[] | — | yes |  |
| `failed` | string[] | — | yes |  |
| `ready` | string[] | — | yes |  |
| `root_id` | string | — | yes |  |
| `root_task` | string | — | yes |  |
| `running` | string[] | — | yes |  |
| `state` | WorkflowState | — | yes |  |
| `tasks` | WorkflowTaskView[] | — | yes |  |

### `workflow.create`

Create a runtime DAG of catalog tasks with per-instance dependencies. Idempotent on `idempotency_key`.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `idempotency_key` | string | — | yes |  |
| `root_id` | string (optional) | null | no |  |
| `tasks` | WorkflowCreateTask[] (optional) | null | no |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `root_id` | string | — | yes |  |
| `tasks` | WorkflowNodeRef[] | — | yes |  |

### `workflow.spawn`

Add one runtime task to an existing workflow root, optionally depending on existing task ids.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `dedupe_key` | string (optional) | null | no |  |
| `depends_on` | string[] (optional) | null | no |  |
| `input` | any | null | no |  |
| `name` | string | — | yes |  |
| `root_id` | string (optional) | null | no |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `attempt` | integer | `0` | no | How many execution attempts this task has had. Starts at 0; each claim increments it and records a [`TaskRun`] under the new attempt. Defaults to 0 so rows and payloads written before attempts existed decode unchanged. |
| `created_at` | string | — | yes |  |
| `dedupe_key` | string (optional) | — | no | Optional dedupe key. Inserting a second task with the same key is a no-op. |
| `error` | string (optional) | — | no | Human-readable failure reason when `status == Failed`. |
| `failure` | Failure (optional) | — | no | Machine-readable failure classification when `status == Failed`. Absent on success and on rows/payloads written before typed failures existed. |
| `finished_at` | string (optional) | — | no |  |
| `id` | string | — | yes |  |
| `input` | any | — | yes | Arbitrary input passed to the task's prompt. |
| `interactive` | boolean | `false` | no | True when the run was started by a user and should execute in the agent's interactive TUI (the Agent panel attaches to it live). Programmatic starts (scheduler, webhooks, hooks, `needs`, `spawn`) leave this false and run headless. Defaults to false so older payloads and rows decode unchanged. |
| `name` | string | — | yes | Name of the catalog task (the `*.md` file) that defines how to run this. |
| `output` | any | — | no | Produced by the task on completion. Omitted from list and push payloads; fetch it on demand with `tasks.get`. |
| `parent_id` | string (optional) | — | no | The task that directly enqueued this one (a `spawn` parent, a `needs` predecessor, …). `None` for a task started directly (manual, scheduled, RPC, hook, webhook). |
| `root_id` | string (optional) | — | no | The workflow origin this run belongs to. A directly-started task has no root of its own (it *is* the root, so [`Task::root_or_self`] falls back to its id); a spawned child inherits its parent's root. |
| `session_id` | string (optional) | — | no | The agent's own session id (e.g. opencode's session id), captured from a task run's output so the session can be reattached later. |
| `session_title` | string (optional) | — | no | The agent's human-readable session title (e.g. opencode's generated title), resolved once at run completion from the agent's session store. Absent until a run finishes or when the agent reports no title. |
| `started_at` | string (optional) | — | no |  |
| `status` | TaskStatus | — | yes |  |

### `workflow.cancel`

Cancel every non-terminal task in a workflow root, emitting `TaskCancelled` per task.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `root_id` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `cancelled` | string[] | — | yes |  |
| `root_id` | string | — | yes |  |

### `workflow.retry`

Retry a terminal task, preserving its prior run history. Single-task alias of `tasks.retry`.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `task_id` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `attempt` | integer | `0` | no | How many execution attempts this task has had. Starts at 0; each claim increments it and records a [`TaskRun`] under the new attempt. Defaults to 0 so rows and payloads written before attempts existed decode unchanged. |
| `created_at` | string | — | yes |  |
| `dedupe_key` | string (optional) | — | no | Optional dedupe key. Inserting a second task with the same key is a no-op. |
| `error` | string (optional) | — | no | Human-readable failure reason when `status == Failed`. |
| `failure` | Failure (optional) | — | no | Machine-readable failure classification when `status == Failed`. Absent on success and on rows/payloads written before typed failures existed. |
| `finished_at` | string (optional) | — | no |  |
| `id` | string | — | yes |  |
| `input` | any | — | yes | Arbitrary input passed to the task's prompt. |
| `interactive` | boolean | `false` | no | True when the run was started by a user and should execute in the agent's interactive TUI (the Agent panel attaches to it live). Programmatic starts (scheduler, webhooks, hooks, `needs`, `spawn`) leave this false and run headless. Defaults to false so older payloads and rows decode unchanged. |
| `name` | string | — | yes | Name of the catalog task (the `*.md` file) that defines how to run this. |
| `output` | any | — | no | Produced by the task on completion. Omitted from list and push payloads; fetch it on demand with `tasks.get`. |
| `parent_id` | string (optional) | — | no | The task that directly enqueued this one (a `spawn` parent, a `needs` predecessor, …). `None` for a task started directly (manual, scheduled, RPC, hook, webhook). |
| `root_id` | string (optional) | — | no | The workflow origin this run belongs to. A directly-started task has no root of its own (it *is* the root, so [`Task::root_or_self`] falls back to its id); a spawned child inherits its parent's root. |
| `session_id` | string (optional) | — | no | The agent's own session id (e.g. opencode's session id), captured from a task run's output so the session can be reattached later. |
| `session_title` | string (optional) | — | no | The agent's human-readable session title (e.g. opencode's generated title), resolved once at run completion from the agent's session store. Absent until a run finishes or when the agent reports no title. |
| `started_at` | string (optional) | — | no |  |
| `status` | TaskStatus | — | yes |  |

### `events.tail`

Tail persisted events.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `limit` | integer (optional) | null | no |  |

**Response**

Event[]

### `events.subscribe`

(Re)subscribe to the live event stream; accepts `last_event_id` to replay missed events before switching to live delivery.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `last_event_id` | integer (optional) | null | no |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `subscribed` | boolean | — | yes |  |

### `agents.list`

List configured external agents and live agent sessions, including each agent's `available` flag and capability flags.

**Request**

object

**Response**

AgentCatalogEntry[]

### `agents.start`

Start an external agent session (optionally attached to a task).

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `agent` | string (optional) | null | no |  |
| `cols` | integer (optional) | null | no |  |
| `cwd` | string (optional) | null | no |  |
| `model` | string (optional) | null | no |  |
| `new` | boolean (optional) | null | no |  |
| `prompt` | string (optional) | null | no |  |
| `provider` | string (optional) | null | no |  |
| `rows` | integer (optional) | null | no |  |
| `session_id` | string (optional) | null | no |  |
| `task_id` | string (optional) | null | no |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `data` | string | — | yes | Base64-encoded `vt100` screen frame. |
| `session` | AgentSessionInfo | — | yes |  |

### `agents.input`

Write raw bytes (base64) to a session's PTY.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `data` | string (optional) | null | no |  |
| `session_id` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `bytes` | integer | — | yes |  |

### `agents.resize`

Resize a session's PTY.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `cols` | integer (optional) | null | no |  |
| `rows` | integer (optional) | null | no |  |
| `session_id` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `cols` | integer | — | yes |  |
| `rows` | integer | — | yes |  |

### `agents.attach`

Attach to a session: returns the session and a replay of its output.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `session_id` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `data` | string | — | yes | Base64-encoded `vt100` screen frame. |
| `session` | AgentSessionInfo | — | yes |  |

### `agents.close`

Terminate a session.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `session_id` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `closed` | string | — | yes |  |

### `agents.reply`

Answer a structured input request on a session's state channel.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `reply` | InputReply | — | yes |  |
| `request_id` | string | — | yes |  |
| `session_id` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `replied` | boolean | — | yes |  |

### `providers.list`

List configured providers and their available models.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `agent` | string (optional) | null | no |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `providers` | ProviderInfo[] | — | yes |  |

### `schedules.list`

List cron schedules.

**Request**

object

**Response**

Schedule[]

### `schedules.upsert`

Create or update a cron schedule.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `cron` | string | — | yes |  |
| `enabled` | boolean (optional) | null | no |  |
| `id` | string (optional) | null | no |  |
| `input` | any | null | no |  |
| `task` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `cron` | string | — | yes |  |
| `enabled` | boolean | — | yes |  |
| `id` | string | — | yes |  |
| `input` | any | — | yes |  |
| `last_run` | string (optional) | — | no |  |
| `task` | string | — | yes | The catalog task name to enqueue on each fire. |

### `schedules.delete`

Delete a cron schedule.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `id` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `deleted` | string | — | yes |  |

### `notifications.list`

List recent notifications.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `limit` | integer (optional) | null | no |  |

**Response**

NotificationRecord[]

### `notifications.test`

Send a test notification through a channel.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `body` | string (optional) | null | no |  |
| `channel` | string | — | yes |  |
| `config` | any | null | no |  |
| `subject` | string (optional) | null | no |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `sent` | boolean | — | yes |  |

### `hooks.upsert`

Add a notification hook reacting to an event kind.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `channel` | string | — | yes |  |
| `config` | any | null | no |  |
| `event` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `added` | boolean | — | yes |  |

### `usage.stats`

Aggregate persisted per-run token/cost usage into a per-bucket series (day/week/month/year) plus window totals.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `period` | UsagePeriod (optional) | null | no |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `buckets` | UsageBucket[] | — | yes | Oldest bucket first. Empty buckets are included so the TUI chart always has a fixed number of points for the selected period. |
| `period` | UsagePeriod | — | yes |  |
| `totals` | UsageTotals | — | yes |  |

## Server → client pushes

| Method | Purpose |
|--------|---------|
| `event` | A persisted event. |
| `task.updated` | A task row changed. |
| `catalog.updated` | The task catalog changed on disk; clients should re-fetch it. |
| `agent.output` | Raw PTY output (base64) from a running agent session. |
| `agent.exit` | An agent session's child process exited. |
| `agent.state` | A session's folded live state (activity/usage) changed. |

## Server → client push payloads

Notifications carry the same typed payloads as the matching `favetto-core` types.

### `event`

A persisted event.

**Payload**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `created_at` | string | — | yes |  |
| `id` | integer | — | yes |  |
| `kind` | EventKind | — | yes |  |
| `payload` | any | — | yes |  |

### `task.updated`

A task row changed.

**Payload**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `attempt` | integer | `0` | no | How many execution attempts this task has had. Starts at 0; each claim increments it and records a [`TaskRun`] under the new attempt. Defaults to 0 so rows and payloads written before attempts existed decode unchanged. |
| `created_at` | string | — | yes |  |
| `dedupe_key` | string (optional) | — | no | Optional dedupe key. Inserting a second task with the same key is a no-op. |
| `error` | string (optional) | — | no | Human-readable failure reason when `status == Failed`. |
| `failure` | Failure (optional) | — | no | Machine-readable failure classification when `status == Failed`. Absent on success and on rows/payloads written before typed failures existed. |
| `finished_at` | string (optional) | — | no |  |
| `id` | string | — | yes |  |
| `input` | any | — | yes | Arbitrary input passed to the task's prompt. |
| `interactive` | boolean | `false` | no | True when the run was started by a user and should execute in the agent's interactive TUI (the Agent panel attaches to it live). Programmatic starts (scheduler, webhooks, hooks, `needs`, `spawn`) leave this false and run headless. Defaults to false so older payloads and rows decode unchanged. |
| `name` | string | — | yes | Name of the catalog task (the `*.md` file) that defines how to run this. |
| `output` | any | — | no | Produced by the task on completion. Omitted from list and push payloads; fetch it on demand with `tasks.get`. |
| `parent_id` | string (optional) | — | no | The task that directly enqueued this one (a `spawn` parent, a `needs` predecessor, …). `None` for a task started directly (manual, scheduled, RPC, hook, webhook). |
| `root_id` | string (optional) | — | no | The workflow origin this run belongs to. A directly-started task has no root of its own (it *is* the root, so [`Task::root_or_self`] falls back to its id); a spawned child inherits its parent's root. |
| `session_id` | string (optional) | — | no | The agent's own session id (e.g. opencode's session id), captured from a task run's output so the session can be reattached later. |
| `session_title` | string (optional) | — | no | The agent's human-readable session title (e.g. opencode's generated title), resolved once at run completion from the agent's session store. Absent until a run finishes or when the agent reports no title. |
| `started_at` | string (optional) | — | no |  |
| `status` | TaskStatus | — | yes |  |

### `catalog.updated`

The task catalog changed on disk; clients should re-fetch it.

**Payload**

object

### `agent.output`

Raw PTY output (base64) from a running agent session.

**Payload**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `data` | string | — | yes | Base64-encoded `vt100` screen frame. |
| `session_id` | string | — | yes |  |

### `agent.exit`

An agent session's child process exited.

**Payload**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `code` | integer (optional) | — | no | Process exit code, when one was reported. |
| `session_id` | string | — | yes |  |

### `agent.state`

A session's folded live state (activity/usage) changed.

**Payload**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `activity` | AgentActivity (optional) | — | no |  |
| `session_id` | string | — | yes |  |
| `usage` | AgentUsage (optional) | — | no |  |

## Error codes

| Code | Meaning |
|------|---------|
| `-32700` | Invalid JSON / MessagePack frame. |
| `-32600` | Not a valid request object. |
| `-32601` | Unknown method. |
| `-32602` | Invalid method parameters. |
| `-32603` | Internal server error. |
| `-32001` | Missing or rejected bearer token. |

## Example

A request is a single MessagePack map; in JSON it looks like:

```json
{"type":"request","id":1,"method":"tasks.start","params":{"name":"hello"}}
```

and the reply:

```json
{"type":"response","id":1,"result":{"task_id":"…","status":"idle"}}
```

Subscribing with `events.subscribe` and `last_event_id` replays persisted events before switching to live delivery, so a TUI that reconnects never misses a task result.

## HTTP endpoints

The daemon also serves HTTP endpoints alongside the RPC transports:

- `GET /metrics` — Prometheus metrics.
- `POST /pair/generate` and `POST /pair/exchange` — pairing.
- `POST /webhooks/github` — GitHub webhook receiver (see [Webhooks & hooks](../guide/webhooks)).

