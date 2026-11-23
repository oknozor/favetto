# Remote API

Generated from `crates/favetto-wire/src/rpc/` by the docs generator. Do not edit by hand.

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
| `config.validate` | Validate the daemon's on-disk config file. |
| `tasks.list` | List tasks (metadata only; output is omitted). |
| `tasks.get` | Fetch a single task by id, including its stored output. |
| `tasks.runs` | List a task's execution attempts (run history). |
| `tasks.events` | Backfill a task's normalized agent-event feed from its session's bounded ring buffer (non-durable; `after`/`next_cursor` are per-session `seq`s). |
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
| `agents.history` | List the durable ACP sessions Favetto created, by Favetto-local id, with title, working directory, timestamps, lifecycle and loadability. Never lists sessions created outside Favetto. |
| `agents.start` | Start an external agent session, or attach to the task's existing live session. `new` forces a fresh session. |
| `agents.load` | Load a persisted ACP session by its Favetto-local id (`session_id`). The provider's ACP id is resolved from the durable record; an unsupported or failed restore is an error and never starts a fresh session. |
| `agents.prompt` | Send a follow-up prompt to a structured (ACP) agent session. |
| `agents.cancel` | Interrupt the in-flight turn of a structured (ACP) agent session. |
| `agents.attach` | Attach to a session: returns the session and a replay of its output. |
| `agents.close` | Terminate a session. |
| `agents.reply` | Answer a structured input request on a session's state channel. |
| `agents.reply_elicitation` | Answer an ACP v1 form elicitation request with accept, decline or cancel. |
| `agents.options` | Read a live ACP session's advertised config options and session modes. The returned snapshot is the same one `agents.list` exposes. |
| `agents.set_option` | Change one config option advertised by a live ACP session. The daemon validates the option id and value against that session's current snapshot and rejects values the agent did not offer. |
| `agents.set_mode` | Switch a live ACP session to one of its advertised modes. Unknown mode ids are rejected; the result carries the reconciled mode/option snapshot. |
| `agents.provider_sessions` | List provider-owned ACP sessions through the agent's advertised `session/list`, with cwd filtering and opaque cursor pagination. Distinct from `agents.history`: rows may never have been created by Favetto. Unsupported agents return `supported: false` and a reason. |
| `agents.provider_resume` | Resume a provider-owned ACP session by its provider ACP id. Prefers `session/load` (with replay) when advertised, then `session/resume`; imports the session into a Favetto-local record and returns that local id for all subsequent calls. |
| `agents.provider_delete` | Delete a provider-owned ACP session through the advertised `session/delete`. Only the provider transcript is deleted; Favetto-local metadata is untouched. |
| `agents.authenticate` | Complete an agent-handled ACP v1 sign-in. Only methods advertised by the agent are accepted; terminal methods are rejected because the daemon cannot run an interactive login process for a remote client. |
| `agents.logout` | Sign a session out through ACP `logout` when the agent advertised it. |
| `providers.list` | List configured providers and their available models. |
| `schedules.list` | List cron schedules, with the next occurrence (`next_run`, UTC) computed for enabled entries. |
| `schedules.upsert` | Create or update a cron schedule. |
| `schedules.delete` | Delete a cron schedule. |
| `notifications.list` | List recent notifications. |
| `notifications.test` | Send a test notification through a channel. |
| `hooks.upsert` | Add a notification hook reacting to an event kind. |
| `usage.stats` | Aggregate persisted per-run token/cost usage into a per-bucket series (day/week/month/year) plus window totals. |
| `repos.list` | List known repositories and the daemon-wide active one. |
| `repos.add` | Register a repository (by daemon-host path) as a known repo. |
| `repos.remove` | Remove a known repository by name. |
| `repos.activate` | Set (or clear) the daemon-wide active repository. |
| `repos.refresh` | Re-scan the base directory and reconcile the repository registry. |

## Client → server request & response types

Every method's `params` and `result` are typed in `favetto-core::rpc::messages`. The tables below are generated from those types' JSON Schemas.

### `system.ping`

Liveness check.

**Request**

object

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `pong` | boolean | — | yes | Always `true` for a live daemon. |
| `cwd` | string | — | yes | The daemon process's current working directory. |

### `config.validate`

Validate the daemon's on-disk config file.

**Request**

object

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `valid` | boolean | — | yes | True when the file parsed, or is absent so defaults apply. |
| `path` | string | — | yes | The resolved config path the daemon was started with. |
| `errors` | string[] | — | yes | One message per parse/deserialization failure; empty when `valid`. |

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
| `id` | string | — | yes |  |
| `name` | string | — | yes | Name of the catalog task (the `*.md` file) that defines how to run this. |
| `status` | TaskStatus | — | yes |  |
| `attempt` | integer | `0` | no | How many execution attempts this task has had. Starts at 0; each claim increments it and records a [`TaskRun`] under the new attempt. Defaults to 0 so rows and payloads written before attempts existed decode unchanged. |
| `input` | any | — | yes | Arbitrary input passed to the task's prompt. |
| `output` | any | — | no | Produced by the task on completion. Omitted from list and push payloads; fetch it on demand with `tasks.get`. |
| `dedupe_key` | string (optional) | — | no | Optional dedupe key. Inserting a second task with the same key is a no-op. |
| `created_at` | string | — | yes |  |
| `started_at` | string (optional) | — | no |  |
| `finished_at` | string (optional) | — | no |  |
| `error` | string (optional) | — | no | Human-readable failure reason when `status == Failed`. |
| `failure` | Failure (optional) | — | no | Machine-readable failure classification when `status == Failed`. Absent on success and on rows/payloads written before typed failures existed. |
| `session_id` | string (optional) | — | no | The agent's own session id (e.g. opencode's session id), captured from a task run's output so the session can be reattached later. |
| `session_title` | string (optional) | — | no | The agent's human-readable session title (e.g. opencode's generated title), resolved once at run completion from the agent's session store. Absent until a run finishes or when the agent reports no title. |
| `parent_id` | string (optional) | — | no | The task that directly enqueued this one (a `spawn` parent, a `needs` predecessor, …). `None` for a task started directly (manual, scheduled, RPC, hook, webhook). |
| `root_id` | string (optional) | — | no | The workflow origin this run belongs to. A directly-started task has no root of its own (it *is* the root, so [`Task::root_or_self`] falls back to its id); a spawned child inherits its parent's root. |
| `interactive` | boolean | `false` | no | True when the run was started by a user and should execute in the agent's interactive TUI (the Agent panel attaches to it live). Programmatic starts (scheduler, webhooks, hooks, `needs`, `spawn`) leave this false and run headless. Defaults to false so older payloads and rows decode unchanged. |
| `agent` | string (optional) | — | no | Optional per-launch agent override. A manual launch can run the task with a different configured agent than the catalog definition pins. The executor resolves it before the definition's `agent` and the global `[agent].default`. Absent for tasks that use the definition default. |
| `provider` | string (optional) | — | no | Optional per-launch model provider override. Resolved atomically with [`Self::model`]: an override of either field never mixes with the definition's provider/model pair. |
| `model` | string (optional) | — | no | Optional per-launch model id override (a bare id or `provider/model`). Absent for tasks that use the definition's or the agent's default model. |

### `tasks.runs`

List a task's execution attempts (run history).

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `id` | string | — | yes |  |

**Response**

TaskRun[]

### `tasks.events`

Backfill a task's normalized agent-event feed from its session's bounded ring buffer (non-durable; `after`/`next_cursor` are per-session `seq`s).

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `id` | string | — | yes |  |
| `after` | integer (optional) | null | no |  |
| `limit` | integer (optional) | null | no |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `session_id` | string (optional) | — | no |  |
| `events` | AgentEventPush[] | — | yes |  |
| `next_cursor` | integer | — | yes |  |
| `plan` | AgentPlanEntry[] (optional) | — | no | The session's current plan at `next_cursor`, when it has one. `None` means there is no structured session; `Some(empty)` means the agent explicitly cleared the plan. |
| `commands` | AgentCommand[] (optional) | — | no | The session's current command list at `next_cursor`, with the same `None`/empty distinction as [`Self::plan`]. |

### `tasks.cancel`

Cancel a task.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `id` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `id` | string | — | yes |  |
| `name` | string | — | yes | Name of the catalog task (the `*.md` file) that defines how to run this. |
| `status` | TaskStatus | — | yes |  |
| `attempt` | integer | `0` | no | How many execution attempts this task has had. Starts at 0; each claim increments it and records a [`TaskRun`] under the new attempt. Defaults to 0 so rows and payloads written before attempts existed decode unchanged. |
| `input` | any | — | yes | Arbitrary input passed to the task's prompt. |
| `output` | any | — | no | Produced by the task on completion. Omitted from list and push payloads; fetch it on demand with `tasks.get`. |
| `dedupe_key` | string (optional) | — | no | Optional dedupe key. Inserting a second task with the same key is a no-op. |
| `created_at` | string | — | yes |  |
| `started_at` | string (optional) | — | no |  |
| `finished_at` | string (optional) | — | no |  |
| `error` | string (optional) | — | no | Human-readable failure reason when `status == Failed`. |
| `failure` | Failure (optional) | — | no | Machine-readable failure classification when `status == Failed`. Absent on success and on rows/payloads written before typed failures existed. |
| `session_id` | string (optional) | — | no | The agent's own session id (e.g. opencode's session id), captured from a task run's output so the session can be reattached later. |
| `session_title` | string (optional) | — | no | The agent's human-readable session title (e.g. opencode's generated title), resolved once at run completion from the agent's session store. Absent until a run finishes or when the agent reports no title. |
| `parent_id` | string (optional) | — | no | The task that directly enqueued this one (a `spawn` parent, a `needs` predecessor, …). `None` for a task started directly (manual, scheduled, RPC, hook, webhook). |
| `root_id` | string (optional) | — | no | The workflow origin this run belongs to. A directly-started task has no root of its own (it *is* the root, so [`Task::root_or_self`] falls back to its id); a spawned child inherits its parent's root. |
| `interactive` | boolean | `false` | no | True when the run was started by a user and should execute in the agent's interactive TUI (the Agent panel attaches to it live). Programmatic starts (scheduler, webhooks, hooks, `needs`, `spawn`) leave this false and run headless. Defaults to false so older payloads and rows decode unchanged. |
| `agent` | string (optional) | — | no | Optional per-launch agent override. A manual launch can run the task with a different configured agent than the catalog definition pins. The executor resolves it before the definition's `agent` and the global `[agent].default`. Absent for tasks that use the definition default. |
| `provider` | string (optional) | — | no | Optional per-launch model provider override. Resolved atomically with [`Self::model`]: an override of either field never mixes with the definition's provider/model pair. |
| `model` | string (optional) | — | no | Optional per-launch model id override (a bare id or `provider/model`). Absent for tasks that use the definition's or the agent's default model. |

### `tasks.retry`

Retry a terminal task, preserving its prior run history.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `id` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `id` | string | — | yes |  |
| `name` | string | — | yes | Name of the catalog task (the `*.md` file) that defines how to run this. |
| `status` | TaskStatus | — | yes |  |
| `attempt` | integer | `0` | no | How many execution attempts this task has had. Starts at 0; each claim increments it and records a [`TaskRun`] under the new attempt. Defaults to 0 so rows and payloads written before attempts existed decode unchanged. |
| `input` | any | — | yes | Arbitrary input passed to the task's prompt. |
| `output` | any | — | no | Produced by the task on completion. Omitted from list and push payloads; fetch it on demand with `tasks.get`. |
| `dedupe_key` | string (optional) | — | no | Optional dedupe key. Inserting a second task with the same key is a no-op. |
| `created_at` | string | — | yes |  |
| `started_at` | string (optional) | — | no |  |
| `finished_at` | string (optional) | — | no |  |
| `error` | string (optional) | — | no | Human-readable failure reason when `status == Failed`. |
| `failure` | Failure (optional) | — | no | Machine-readable failure classification when `status == Failed`. Absent on success and on rows/payloads written before typed failures existed. |
| `session_id` | string (optional) | — | no | The agent's own session id (e.g. opencode's session id), captured from a task run's output so the session can be reattached later. |
| `session_title` | string (optional) | — | no | The agent's human-readable session title (e.g. opencode's generated title), resolved once at run completion from the agent's session store. Absent until a run finishes or when the agent reports no title. |
| `parent_id` | string (optional) | — | no | The task that directly enqueued this one (a `spawn` parent, a `needs` predecessor, …). `None` for a task started directly (manual, scheduled, RPC, hook, webhook). |
| `root_id` | string (optional) | — | no | The workflow origin this run belongs to. A directly-started task has no root of its own (it *is* the root, so [`Task::root_or_self`] falls back to its id); a spawned child inherits its parent's root. |
| `interactive` | boolean | `false` | no | True when the run was started by a user and should execute in the agent's interactive TUI (the Agent panel attaches to it live). Programmatic starts (scheduler, webhooks, hooks, `needs`, `spawn`) leave this false and run headless. Defaults to false so older payloads and rows decode unchanged. |
| `agent` | string (optional) | — | no | Optional per-launch agent override. A manual launch can run the task with a different configured agent than the catalog definition pins. The executor resolves it before the definition's `agent` and the global `[agent].default`. Absent for tasks that use the definition default. |
| `provider` | string (optional) | — | no | Optional per-launch model provider override. Resolved atomically with [`Self::model`]: an override of either field never mixes with the definition's provider/model pair. |
| `model` | string (optional) | — | no | Optional per-launch model id override (a bare id or `provider/model`). Absent for tasks that use the definition's or the agent's default model. |

### `tasks.start`

Start a catalog task by name.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `name` | string | — | yes |  |
| `input` | any | null | no |  |
| `interactive` | boolean (optional) | null | no |  |
| `agent` | string (optional) | null | no |  |
| `provider` | string (optional) | null | no |  |
| `model` | string (optional) | null | no |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `id` | string | — | yes |  |
| `name` | string | — | yes | Name of the catalog task (the `*.md` file) that defines how to run this. |
| `status` | TaskStatus | — | yes |  |
| `attempt` | integer | `0` | no | How many execution attempts this task has had. Starts at 0; each claim increments it and records a [`TaskRun`] under the new attempt. Defaults to 0 so rows and payloads written before attempts existed decode unchanged. |
| `input` | any | — | yes | Arbitrary input passed to the task's prompt. |
| `output` | any | — | no | Produced by the task on completion. Omitted from list and push payloads; fetch it on demand with `tasks.get`. |
| `dedupe_key` | string (optional) | — | no | Optional dedupe key. Inserting a second task with the same key is a no-op. |
| `created_at` | string | — | yes |  |
| `started_at` | string (optional) | — | no |  |
| `finished_at` | string (optional) | — | no |  |
| `error` | string (optional) | — | no | Human-readable failure reason when `status == Failed`. |
| `failure` | Failure (optional) | — | no | Machine-readable failure classification when `status == Failed`. Absent on success and on rows/payloads written before typed failures existed. |
| `session_id` | string (optional) | — | no | The agent's own session id (e.g. opencode's session id), captured from a task run's output so the session can be reattached later. |
| `session_title` | string (optional) | — | no | The agent's human-readable session title (e.g. opencode's generated title), resolved once at run completion from the agent's session store. Absent until a run finishes or when the agent reports no title. |
| `parent_id` | string (optional) | — | no | The task that directly enqueued this one (a `spawn` parent, a `needs` predecessor, …). `None` for a task started directly (manual, scheduled, RPC, hook, webhook). |
| `root_id` | string (optional) | — | no | The workflow origin this run belongs to. A directly-started task has no root of its own (it *is* the root, so [`Task::root_or_self`] falls back to its id); a spawned child inherits its parent's root. |
| `interactive` | boolean | `false` | no | True when the run was started by a user and should execute in the agent's interactive TUI (the Agent panel attaches to it live). Programmatic starts (scheduler, webhooks, hooks, `needs`, `spawn`) leave this false and run headless. Defaults to false so older payloads and rows decode unchanged. |
| `agent` | string (optional) | — | no | Optional per-launch agent override. A manual launch can run the task with a different configured agent than the catalog definition pins. The executor resolves it before the definition's `agent` and the global `[agent].default`. Absent for tasks that use the definition default. |
| `provider` | string (optional) | — | no | Optional per-launch model provider override. Resolved atomically with [`Self::model`]: an override of either field never mixes with the definition's provider/model pair. |
| `model` | string (optional) | — | no | Optional per-launch model id override (a bare id or `provider/model`). Absent for tasks that use the definition's or the agent's default model. |

### `tasks.start_oneshot`

Start a one-shot task from an inline definition (not added to the catalog) and open its interactive agent session.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `agent` | string (optional) | null | no |  |
| `provider` | string (optional) | null | no |  |
| `model` | string (optional) | null | no |  |
| `cwd` | string (optional) | null | no |  |
| `rows` | integer (optional) | null | no |  |
| `cols` | integer (optional) | null | no |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `task` | Task | — | yes |  |
| `session` | AgentSessionInfo | — | yes |  |
| `data` | string | — | yes | Base64-encoded `vt100` screen frame. |
| `seq` | integer | `0` | no | See [`AgentAttachResult::seq`]. |

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
| `name` | string | — | yes |  |
| `agent` | string (optional) | null | no |  |
| `provider` | string (optional) | null | no |  |
| `model` | string (optional) | null | no |  |
| `cwd` | string (optional) | null | no |  |
| `schedule` | string (optional) | null | no |  |
| `needs` | string (optional) | null | no |  |
| `spawn` | string (optional) | null | no |  |
| `spawn_file` | string (optional) | null | no |  |
| `spawn_new_root` | boolean (optional) | null | no |  |
| `prompt` | string (optional) | null | no |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `name` | string | — | yes |  |
| `agent` | string (optional) | — | no |  |
| `provider` | string (optional) | — | no |  |
| `model` | string (optional) | — | no |  |
| `cwd` | string (optional) | — | no |  |
| `schedule` | string (optional) | — | no |  |
| `needs` | string (optional) | — | no |  |
| `vars` | TaskVar[] | — | yes |  |

### `catalog.update`

Replace an existing catalog task's raw Markdown source (does not run it).

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `name` | string | — | yes |  |
| `markdown` | string | — | yes |  |

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
| `path` | string | — | yes |  |
| `graph` | WorkflowGraph | — | yes |  |

### `workflow.inspect`

Fetch the runtime workflow graph for a root (task instances plus ready/running/failed/blocked buckets).

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `root_id` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `root_id` | string | — | yes |  |
| `root_task` | string | — | yes |  |
| `state` | WorkflowState | — | yes |  |
| `tasks` | WorkflowTaskView[] | — | yes |  |
| `ready` | string[] | — | yes |  |
| `running` | string[] | — | yes |  |
| `failed` | string[] | — | yes |  |
| `blocked` | string[] | — | yes |  |

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
| `name` | string | — | yes |  |
| `input` | any | null | no |  |
| `root_id` | string (optional) | null | no |  |
| `depends_on` | string[] (optional) | null | no |  |
| `dedupe_key` | string (optional) | null | no |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `id` | string | — | yes |  |
| `name` | string | — | yes | Name of the catalog task (the `*.md` file) that defines how to run this. |
| `status` | TaskStatus | — | yes |  |
| `attempt` | integer | `0` | no | How many execution attempts this task has had. Starts at 0; each claim increments it and records a [`TaskRun`] under the new attempt. Defaults to 0 so rows and payloads written before attempts existed decode unchanged. |
| `input` | any | — | yes | Arbitrary input passed to the task's prompt. |
| `output` | any | — | no | Produced by the task on completion. Omitted from list and push payloads; fetch it on demand with `tasks.get`. |
| `dedupe_key` | string (optional) | — | no | Optional dedupe key. Inserting a second task with the same key is a no-op. |
| `created_at` | string | — | yes |  |
| `started_at` | string (optional) | — | no |  |
| `finished_at` | string (optional) | — | no |  |
| `error` | string (optional) | — | no | Human-readable failure reason when `status == Failed`. |
| `failure` | Failure (optional) | — | no | Machine-readable failure classification when `status == Failed`. Absent on success and on rows/payloads written before typed failures existed. |
| `session_id` | string (optional) | — | no | The agent's own session id (e.g. opencode's session id), captured from a task run's output so the session can be reattached later. |
| `session_title` | string (optional) | — | no | The agent's human-readable session title (e.g. opencode's generated title), resolved once at run completion from the agent's session store. Absent until a run finishes or when the agent reports no title. |
| `parent_id` | string (optional) | — | no | The task that directly enqueued this one (a `spawn` parent, a `needs` predecessor, …). `None` for a task started directly (manual, scheduled, RPC, hook, webhook). |
| `root_id` | string (optional) | — | no | The workflow origin this run belongs to. A directly-started task has no root of its own (it *is* the root, so [`Task::root_or_self`] falls back to its id); a spawned child inherits its parent's root. |
| `interactive` | boolean | `false` | no | True when the run was started by a user and should execute in the agent's interactive TUI (the Agent panel attaches to it live). Programmatic starts (scheduler, webhooks, hooks, `needs`, `spawn`) leave this false and run headless. Defaults to false so older payloads and rows decode unchanged. |
| `agent` | string (optional) | — | no | Optional per-launch agent override. A manual launch can run the task with a different configured agent than the catalog definition pins. The executor resolves it before the definition's `agent` and the global `[agent].default`. Absent for tasks that use the definition default. |
| `provider` | string (optional) | — | no | Optional per-launch model provider override. Resolved atomically with [`Self::model`]: an override of either field never mixes with the definition's provider/model pair. |
| `model` | string (optional) | — | no | Optional per-launch model id override (a bare id or `provider/model`). Absent for tasks that use the definition's or the agent's default model. |

### `workflow.cancel`

Cancel every non-terminal task in a workflow root, emitting `TaskCancelled` per task.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `root_id` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `root_id` | string | — | yes |  |
| `cancelled` | string[] | — | yes |  |

### `workflow.retry`

Retry a terminal task, preserving its prior run history. Single-task alias of `tasks.retry`.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `task_id` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `id` | string | — | yes |  |
| `name` | string | — | yes | Name of the catalog task (the `*.md` file) that defines how to run this. |
| `status` | TaskStatus | — | yes |  |
| `attempt` | integer | `0` | no | How many execution attempts this task has had. Starts at 0; each claim increments it and records a [`TaskRun`] under the new attempt. Defaults to 0 so rows and payloads written before attempts existed decode unchanged. |
| `input` | any | — | yes | Arbitrary input passed to the task's prompt. |
| `output` | any | — | no | Produced by the task on completion. Omitted from list and push payloads; fetch it on demand with `tasks.get`. |
| `dedupe_key` | string (optional) | — | no | Optional dedupe key. Inserting a second task with the same key is a no-op. |
| `created_at` | string | — | yes |  |
| `started_at` | string (optional) | — | no |  |
| `finished_at` | string (optional) | — | no |  |
| `error` | string (optional) | — | no | Human-readable failure reason when `status == Failed`. |
| `failure` | Failure (optional) | — | no | Machine-readable failure classification when `status == Failed`. Absent on success and on rows/payloads written before typed failures existed. |
| `session_id` | string (optional) | — | no | The agent's own session id (e.g. opencode's session id), captured from a task run's output so the session can be reattached later. |
| `session_title` | string (optional) | — | no | The agent's human-readable session title (e.g. opencode's generated title), resolved once at run completion from the agent's session store. Absent until a run finishes or when the agent reports no title. |
| `parent_id` | string (optional) | — | no | The task that directly enqueued this one (a `spawn` parent, a `needs` predecessor, …). `None` for a task started directly (manual, scheduled, RPC, hook, webhook). |
| `root_id` | string (optional) | — | no | The workflow origin this run belongs to. A directly-started task has no root of its own (it *is* the root, so [`Task::root_or_self`] falls back to its id); a spawned child inherits its parent's root. |
| `interactive` | boolean | `false` | no | True when the run was started by a user and should execute in the agent's interactive TUI (the Agent panel attaches to it live). Programmatic starts (scheduler, webhooks, hooks, `needs`, `spawn`) leave this false and run headless. Defaults to false so older payloads and rows decode unchanged. |
| `agent` | string (optional) | — | no | Optional per-launch agent override. A manual launch can run the task with a different configured agent than the catalog definition pins. The executor resolves it before the definition's `agent` and the global `[agent].default`. Absent for tasks that use the definition default. |
| `provider` | string (optional) | — | no | Optional per-launch model provider override. Resolved atomically with [`Self::model`]: an override of either field never mixes with the definition's provider/model pair. |
| `model` | string (optional) | — | no | Optional per-launch model id override (a bare id or `provider/model`). Absent for tasks that use the definition's or the agent's default model. |

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

### `agents.history`

List the durable ACP sessions Favetto created, by Favetto-local id, with title, working directory, timestamps, lifecycle and loadability. Never lists sessions created outside Favetto.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `limit` | integer (optional) | null | no |  |

**Response**

AgentSessionHistoryEntry[]

### `agents.start`

Start an external agent session, or attach to the task's existing live session. `new` forces a fresh session.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `agent` | string (optional) | null | no |  |
| `task_id` | string (optional) | null | no |  |
| `prompt` | string (optional) | null | no |  |
| `session_id` | string (optional) | null | no |  |
| `rows` | integer (optional) | null | no |  |
| `cols` | integer (optional) | null | no |  |
| `new` | boolean (optional) | null | no |  |
| `provider` | string (optional) | null | no |  |
| `model` | string (optional) | null | no |  |
| `cwd` | string (optional) | null | no |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `session` | AgentSessionInfo | — | yes |  |
| `data` | string | — | yes | Base64-encoded `vt100` screen frame. |
| `seq` | integer | `0` | no | The last normalized-event sequence already folded into `data`. A client that also consumes `agent.event` / `tasks.events` can resume strictly after this cursor without replaying text that the snapshot already contains. Older payloads omit it and decode as `0`. |
| `plan` | AgentPlanEntry[] | — | no | The session's current execution plan at [`Self::seq`]. Empty when the agent has not reported one; an empty list is also a valid cleared plan. |
| `commands` | AgentCommand[] | — | no | The session's current advertised slash commands at [`Self::seq`]. |

### `agents.load`

Load a persisted ACP session by its Favetto-local id (`session_id`). The provider's ACP id is resolved from the durable record; an unsupported or failed restore is an error and never starts a fresh session.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `session_id` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `session` | AgentSessionInfo | — | yes |  |
| `data` | string | — | yes | Base64-encoded `vt100` screen frame. |
| `seq` | integer | `0` | no | The last normalized-event sequence already folded into `data`. A client that also consumes `agent.event` / `tasks.events` can resume strictly after this cursor without replaying text that the snapshot already contains. Older payloads omit it and decode as `0`. |
| `plan` | AgentPlanEntry[] | — | no | The session's current execution plan at [`Self::seq`]. Empty when the agent has not reported one; an empty list is also a valid cleared plan. |
| `commands` | AgentCommand[] | — | no | The session's current advertised slash commands at [`Self::seq`]. |

### `agents.prompt`

Send a follow-up prompt to a structured (ACP) agent session.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `session_id` | string | — | yes |  |
| `prompt` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `accepted` | boolean | — | yes |  |

### `agents.cancel`

Interrupt the in-flight turn of a structured (ACP) agent session.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `session_id` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `cancelled` | boolean | — | yes |  |

### `agents.attach`

Attach to a session: returns the session and a replay of its output.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `session_id` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `session` | AgentSessionInfo | — | yes |  |
| `data` | string | — | yes | Base64-encoded `vt100` screen frame. |
| `seq` | integer | `0` | no | The last normalized-event sequence already folded into `data`. A client that also consumes `agent.event` / `tasks.events` can resume strictly after this cursor without replaying text that the snapshot already contains. Older payloads omit it and decode as `0`. |
| `plan` | AgentPlanEntry[] | — | no | The session's current execution plan at [`Self::seq`]. Empty when the agent has not reported one; an empty list is also a valid cleared plan. |
| `commands` | AgentCommand[] | — | no | The session's current advertised slash commands at [`Self::seq`]. |

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
| `session_id` | string | — | yes |  |
| `request_id` | string | — | yes |  |
| `reply` | InputReply | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `replied` | boolean | — | yes |  |

### `agents.reply_elicitation`

Answer an ACP v1 form elicitation request with accept, decline or cancel.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `session_id` | string | — | yes |  |
| `request_id` | string | — | yes |  |
| `reply` | ElicitationReply | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `replied` | boolean | — | yes |  |

### `agents.options`

Read a live ACP session's advertised config options and session modes. The returned snapshot is the same one `agents.list` exposes.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `session_id` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `session_id` | string | — | yes |  |
| `config_options` | SessionConfigOption[] | `[]` | no |  |
| `modes` | SessionModes (optional) | — | no |  |

### `agents.set_option`

Change one config option advertised by a live ACP session. The daemon validates the option id and value against that session's current snapshot and rejects values the agent did not offer.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `session_id` | string | — | yes |  |
| `option_id` | string | — | yes |  |
| `value` | SessionConfigValue | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `session_id` | string | — | yes |  |
| `config_options` | SessionConfigOption[] | `[]` | no |  |
| `modes` | SessionModes (optional) | — | no |  |

### `agents.set_mode`

Switch a live ACP session to one of its advertised modes. Unknown mode ids are rejected; the result carries the reconciled mode/option snapshot.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `session_id` | string | — | yes |  |
| `mode_id` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `session_id` | string | — | yes |  |
| `config_options` | SessionConfigOption[] | `[]` | no |  |
| `modes` | SessionModes (optional) | — | no |  |

### `agents.provider_sessions`

List provider-owned ACP sessions through the agent's advertised `session/list`, with cwd filtering and opaque cursor pagination. Distinct from `agents.history`: rows may never have been created by Favetto. Unsupported agents return `supported: false` and a reason.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `agent` | string (optional) | null | no |  |
| `cwd` | string (optional) | null | no |  |
| `cursor` | string (optional) | null | no |  |

**Response**

ProviderSessionsResult[]

### `agents.provider_resume`

Resume a provider-owned ACP session by its provider ACP id. Prefers `session/load` (with replay) when advertised, then `session/resume`; imports the session into a Favetto-local record and returns that local id for all subsequent calls.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `agent` | string (optional) | null | no |  |
| `provider_session_id` | string | — | yes | The provider's ACP session id. This is not a Favetto-local routing key. |
| `cwd` | string (optional) | null | no | Working directory to resume in. Defaults to the provider session's listed directory when omitted. |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `session` | AgentSessionInfo | — | yes |  |
| `data` | string | — | yes | Base64-encoded `vt100` screen frame. |
| `seq` | integer | `0` | no | The last normalized-event sequence already folded into `data`. A client that also consumes `agent.event` / `tasks.events` can resume strictly after this cursor without replaying text that the snapshot already contains. Older payloads omit it and decode as `0`. |
| `plan` | AgentPlanEntry[] | — | no | The session's current execution plan at [`Self::seq`]. Empty when the agent has not reported one; an empty list is also a valid cleared plan. |
| `commands` | AgentCommand[] | — | no | The session's current advertised slash commands at [`Self::seq`]. |

### `agents.provider_delete`

Delete a provider-owned ACP session through the advertised `session/delete`. Only the provider transcript is deleted; Favetto-local metadata is untouched.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `agent` | string (optional) | null | no |  |
| `provider_session_id` | string | — | yes | The provider's ACP session id. This is not a Favetto-local routing key. |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `deleted` | boolean | — | yes |  |

### `agents.authenticate`

Complete an agent-handled ACP v1 sign-in. Only methods advertised by the agent are accepted; terminal methods are rejected because the daemon cannot run an interactive login process for a remote client.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `session_id` | string | — | yes |  |
| `method_id` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `authenticated` | boolean | — | yes |  |
| `auth` | AgentAuthState | — | yes |  |

### `agents.logout`

Sign a session out through ACP `logout` when the agent advertised it.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `session_id` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `logged_out` | boolean | — | yes |  |

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

List cron schedules, with the next occurrence (`next_run`, UTC) computed for enabled entries.

**Request**

object

**Response**

Schedule[]

### `schedules.upsert`

Create or update a cron schedule.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `id` | string (optional) | null | no |  |
| `cron` | string | — | yes |  |
| `task` | string | — | yes |  |
| `input` | any | null | no |  |
| `enabled` | boolean (optional) | null | no |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `id` | string | — | yes |  |
| `cron` | string | — | yes |  |
| `task` | string | — | yes | The catalog task name to enqueue on each fire. |
| `input` | any | — | yes |  |
| `enabled` | boolean | — | yes |  |
| `last_run` | string (optional) | — | no |  |
| `next_run` | string (optional) | null | no | The next occurrence of `cron` in UTC, computed by the daemon for enabled schedules. `None` when the schedule is disabled, its expression does not parse, or no future occurrence exists. Never persisted; older payloads omit it. |

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
| `channel` | string | — | yes |  |
| `config` | any | null | no |  |
| `subject` | string (optional) | null | no |  |
| `body` | string (optional) | null | no |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `sent` | boolean | — | yes |  |

### `hooks.upsert`

Add a notification hook reacting to an event kind.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `event` | string | — | yes |  |
| `channel` | string | — | yes |  |
| `config` | any | null | no |  |

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
| `period` | UsagePeriod | — | yes |  |
| `totals` | UsageTotals | — | yes |  |
| `buckets` | UsageBucket[] | — | yes | Oldest bucket first. Empty buckets are included so the TUI chart always has a fixed number of points for the selected period. |

### `repos.list`

List known repositories and the daemon-wide active one.

**Request**

object

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `repos` | RepoInfo[] | — | yes |  |
| `active` | string (optional) | null | no |  |

### `repos.add`

Register a repository (by daemon-host path) as a known repo.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `path` | string | — | yes |  |
| `name` | string (optional) | null | no |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `name` | string | — | yes |  |
| `path` | string | — | yes |  |
| `available` | boolean | `true` | no | Whether the repo currently has a `.favetto/tasks/` catalog directory. |
| `active` | boolean | `false` | no | Whether this is the daemon-wide active repo. |
| `source` | RepoSource | `manual` | no | How the repo entered the registry. |

### `repos.remove`

Remove a known repository by name.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `name` | string | — | yes |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `removed` | boolean | — | yes |  |

### `repos.activate`

Set (or clear) the daemon-wide active repository.

**Request**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `name` | string (optional) | null | no |  |

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `repos` | RepoInfo[] | — | yes |  |
| `active` | string (optional) | null | no |  |

### `repos.refresh`

Re-scan the base directory and reconcile the repository registry.

**Request**

object

**Response**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `repos` | RepoInfo[] | — | yes |  |
| `active` | string (optional) | null | no |  |

## Server → client pushes

| Method | Purpose |
|--------|---------|
| `event` | A persisted event. |
| `task.updated` | A task row changed. |
| `catalog.updated` | The task catalog changed on disk; clients should re-fetch it. |
| `agent.exit` | An agent session's child process exited. |
| `agent.state` | A session's folded live state (activity/usage) changed. |
| `agent.event` | One normalized agent-state event, in order. Non-durable; backfill with `tasks.events`. |

## Server → client push payloads

Notifications carry the same typed payloads as the matching `favetto-core` types.

### `event`

A persisted event.

**Payload**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `id` | integer | — | yes |  |
| `kind` | EventKind | — | yes |  |
| `payload` | any | — | yes |  |
| `created_at` | string | — | yes |  |

### `task.updated`

A task row changed.

**Payload**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `id` | string | — | yes |  |
| `name` | string | — | yes | Name of the catalog task (the `*.md` file) that defines how to run this. |
| `status` | TaskStatus | — | yes |  |
| `attempt` | integer | `0` | no | How many execution attempts this task has had. Starts at 0; each claim increments it and records a [`TaskRun`] under the new attempt. Defaults to 0 so rows and payloads written before attempts existed decode unchanged. |
| `input` | any | — | yes | Arbitrary input passed to the task's prompt. |
| `output` | any | — | no | Produced by the task on completion. Omitted from list and push payloads; fetch it on demand with `tasks.get`. |
| `dedupe_key` | string (optional) | — | no | Optional dedupe key. Inserting a second task with the same key is a no-op. |
| `created_at` | string | — | yes |  |
| `started_at` | string (optional) | — | no |  |
| `finished_at` | string (optional) | — | no |  |
| `error` | string (optional) | — | no | Human-readable failure reason when `status == Failed`. |
| `failure` | Failure (optional) | — | no | Machine-readable failure classification when `status == Failed`. Absent on success and on rows/payloads written before typed failures existed. |
| `session_id` | string (optional) | — | no | The agent's own session id (e.g. opencode's session id), captured from a task run's output so the session can be reattached later. |
| `session_title` | string (optional) | — | no | The agent's human-readable session title (e.g. opencode's generated title), resolved once at run completion from the agent's session store. Absent until a run finishes or when the agent reports no title. |
| `parent_id` | string (optional) | — | no | The task that directly enqueued this one (a `spawn` parent, a `needs` predecessor, …). `None` for a task started directly (manual, scheduled, RPC, hook, webhook). |
| `root_id` | string (optional) | — | no | The workflow origin this run belongs to. A directly-started task has no root of its own (it *is* the root, so [`Task::root_or_self`] falls back to its id); a spawned child inherits its parent's root. |
| `interactive` | boolean | `false` | no | True when the run was started by a user and should execute in the agent's interactive TUI (the Agent panel attaches to it live). Programmatic starts (scheduler, webhooks, hooks, `needs`, `spawn`) leave this false and run headless. Defaults to false so older payloads and rows decode unchanged. |
| `agent` | string (optional) | — | no | Optional per-launch agent override. A manual launch can run the task with a different configured agent than the catalog definition pins. The executor resolves it before the definition's `agent` and the global `[agent].default`. Absent for tasks that use the definition default. |
| `provider` | string (optional) | — | no | Optional per-launch model provider override. Resolved atomically with [`Self::model`]: an override of either field never mixes with the definition's provider/model pair. |
| `model` | string (optional) | — | no | Optional per-launch model id override (a bare id or `provider/model`). Absent for tasks that use the definition's or the agent's default model. |

### `catalog.updated`

The task catalog changed on disk; clients should re-fetch it.

**Payload**

object

### `agent.exit`

An agent session's child process exited.

**Payload**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `session_id` | string | — | yes |  |
| `code` | integer (optional) | — | no | Process exit code, when one was reported. |

### `agent.state`

A session's folded live state (activity/usage) changed.

**Payload**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `session_id` | string | — | yes |  |
| `activity` | AgentActivity (optional) | — | no |  |
| `usage` | AgentUsage (optional) | — | no |  |

### `agent.event`

One normalized agent-state event, in order. Non-durable; backfill with `tasks.events`.

**Payload**

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `session_id` | string | — | yes |  |
| `task_id` | string (optional) | — | no |  |
| `seq` | integer | — | yes |  |
| `event` | AgentStateEvent | — | yes |  |

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
- `POST /auth/login` — username/password login (returns the bearer token).
- `POST /pair/generate` (bearer-guarded) and `POST /pair/exchange` — pairing.
- `POST /webhooks/github` — GitHub webhook receiver (see [Webhooks & hooks](../guide/webhooks)).

