# Supervisor contract

A **supervisor** is an external controller — a program, a script, a web UI, a
human, or another agent — that drives a favetto workflow by observing it and
issuing a small, strict vocabulary of decisions. This page is the contract
between that controller and the daemon.

The supervisor is **not** part of the daemon. favetto validates and executes;
the supervisor only expresses intent. The daemon gains no scheduling policy, no
retry heuristics, and no LLM code from this contract: it stays deterministic, and
all reasoning lives outside the process. See the
[architecture boundary](#the-determinism-boundary) below.

The contract is transport-agnostic. A supervisor is simply another remote API
client: the same MessagePack frames over the Unix socket or the authenticated
WebSocket described in the [Remote API reference](./remote-api), using the same
bearer token.

## Observation

The supervisor observes a single **workflow root** — a task and everything
spawned beneath it. Two channels answer the two questions a controller has:

- *What is happening right now?* → `workflow.inspect`
- *What changed since I last looked?* → the event cursor (`events.subscribe`)

### `workflow.inspect`

`workflow.inspect` is a read-only runtime view of a root, distinct from
`workflow.get`, which returns the **catalog** graph (task definitions, not
instances). It never embeds `output` blobs; per-task detail stays behind
[`tasks.get`](#tasksget-and-agentslist).

Request:

```json
{ "root_id": "3f1c…" }
```

Response:

```json
{
  "root_id": "3f1c…",
  "root_task": "implement_issue",
  "state": "running",
  "tasks": [
    { "id": "a1b2…", "name": "research", "status": "succeeded", "attempt": 1,
      "summary": "Found two call sites." },
    { "id": "c3d4…", "name": "implement_issue", "status": "running", "attempt": 2 }
  ],
  "ready": [],
  "running": ["c3d4…"],
  "failed": [],
  "blocked": []
}
```

| Field | Type | Meaning |
|-------|------|---------|
| `root_id` | uuid | The root that was inspected. |
| `root_task` | string | Catalog name of the root task. |
| `state` | `running` \| `succeeded` \| `failed` \| `cancelled` | Overall root state. `running` while any instance is non-terminal; otherwise the worst terminal outcome (failed beats cancelled beats succeeded). |
| `tasks` | array | One [task view](#task-view) per instance in the root, oldest first. |
| `ready` | uuid[] | Pending instances with no unmet dependency: free to run but not yet claimed. |
| `running` | uuid[] | Instances currently `running` or `awaiting_input`. |
| `blocked` | uuid[] | Pending instances waiting on a `needs` predecessor or a runtime `depends_on` edge. |
| `failed` | uuid[] | Instances in `failed`. Succeeded and cancelled instances appear only in `tasks`. |

A missing or empty root is an invalid-params error; the supervisor should treat
it as "no such workflow" rather than retrying.

#### Task view

| Field | Type | Meaning |
|-------|------|---------|
| `id` | uuid | Instance id. Pass it to `tasks.get`, `workflow.retry`, `tasks.cancel`, … |
| `name` | string | Catalog task name. |
| `status` | `pending` \| `running` \| `awaiting_input` \| `succeeded` \| `failed` \| `cancelled` | Lifecycle state of the instance. |
| `attempt` | integer | Run attempt. `0` until the instance is first claimed; its first run is attempt `1` and each retry advances it. |
| `summary` | string, omitted when absent | Bounded outcome text: the error for failures and cancellations. Absent for successes and for instances that have not finished. |

### The event cursor

`workflow.inspect` is a snapshot; the event stream turns it into a live cursor.
On connect, subscribe to the event bus and tell it where you left off:

```json
{ "method": "events.subscribe", "params": { "last_event_id": 4820 } }
```

The daemon acknowledges with `{ "subscribed": true }`, replays persisted events
with `id > last_event_id`, then pushes new ones. Every event arrives as an
`event` notification whose payload is:

```json
{
  "id": 4821,
  "kind": "task_finished",
  "payload": {
    "name": "research",
    "task_id": "a1b2…",
    "success": true,
    "status": "succeeded",
    "attempt": 1,
    "retryable": false,
    "summary": "Found two call sites."
  },
  "created_at": "2026-09-24T09:00:00Z"
}
```

`id` is a monotonically increasing integer — the **cursor**. Persist the
highest `id` you have processed; a reconnect with that `last_event_id` replays
exactly what you missed, so a supervisor survives its own restarts. A live
connection that cannot keep up has excess broadcasts dropped (the daemon logs a
"client fell behind" warning); the supervisor recovers by re-running
`workflow.inspect` and resuming from the last cursor it persisted.

A one-shot backfill without subscribing is available via
`events.tail { "limit": 50 }` (capped at 1000). See the
[Event kinds reference](./events) for the full `kind` vocabulary. The kinds a
supervisor normally reacts to are `task_started`, `task_awaiting_input`,
`task_completed`, `task_failed`, `task_cancelled`, and the
dependency-facing `task_finished`.

### `tasks.get` and `agents.list`

`workflow.inspect` deliberately carries no large payloads. When the supervisor
needs the result envelope of a finished task, it calls
`tasks.get { "id": "c3d4…" }`. When a task is `awaiting_input`, it calls
`agents.list` to read the session's `awaiting_input` reason — `kind`
(`permission`, `confirmation`, `choice`, `pinentry`, `other`), the prompt
`message`, the selectable `options`, and, when the prompt can be answered
precisely, a `request_id`.

## Decision

Each time the supervisor looks, it emits **exactly one** decision object. A
decision is data, not code: it names an action, carries the arguments for that
action, and records why it was chosen. A supervisor for an LLM can emit this
directly as structured output; a scripted supervisor can build it with a
`match`.

```json
{
  "action": "spawn",
  "reason": "The plan is approved and no implementer is in flight.",
  "cursor": 4821,
  "params": { "name": "implement_issue", "input": { "issue": "152" } }
}
```

| Field | Type | Required | Meaning |
|-------|------|----------|---------|
| `action` | string | yes | One of the [eight actions](#action-vocabulary). Anything else is a contract violation the supervisor must not emit. |
| `reason` | string | no (recommended) | Human-readable audit trail. The daemon never reads it. |
| `cursor` | integer | no | The `event.id` the decision was based on. Lets the runtime and logs correlate a decision with the observation that triggered it. |
| `params` | object | no | Action-specific arguments, forwarded to the mapped RPC (see below). |

A supervisor that needs more than one mutation per cycle expresses that
explicitly: it emits one decision, applies it, takes a fresh observation, and
decides again. This keeps the loop auditable and the daemon stateless with
respect to the supervisor.

## Action vocabulary

The vocabulary is closed: `inspect`, `spawn`, `cancel`, `retry`, `wait`,
`request_input`, `complete`, `escalate`. A supervisor must not invent actions;
the four read/mutate actions map onto daemon RPCs, while `wait`, `complete`,
`request_input`, and `escalate` are supervisor-side states that send nothing.

| Action | Intent | Daemon RPC | `params` |
|--------|--------|------------|----------|
| `inspect` | Refresh the observation. | `workflow.inspect` | `{ "root_id": uuid }` |
| `spawn` | Add work to a root. | `workflow.spawn` | `{ "root_id"?: uuid, "name": string, "input"?: json, "depends_on"?: uuid[], "dedupe_key"?: string }` |
| | Create a whole DAG at once. | `workflow.create` | `{ "idempotency_key": string, "root_id"?: uuid, "tasks": [{ "key": string, "name": string, "input"?: json, "depends_on"?: string[] }] }` |
| | Start a brand-new root. | `tasks.start` | `{ "name": string, "input"?: json }` |
| `cancel` | Abandon a whole root. | `workflow.cancel` | `{ "root_id": uuid }` |
| | Abandon a single task. | `tasks.cancel` | `{ "id": uuid }` |
| `retry` | Re-run a terminal task, preserving prior run history. | `workflow.retry` | `{ "task_id": uuid }` |
| | Single-task alias of the above. | `tasks.retry` | `{ "id": uuid }` |
| `wait` | Do nothing; keep observing. | — | `{ "timeout_ms"?: integer, "until"?: string[] }` |
| `request_input` | Ask a human for a decision or value needed to continue. | — | `{ "prompt": string, "task_id"?: uuid }` |
| `complete` | Declare the root finished; stop supervising it. | — | `{ "root_id": uuid, "summary"?: string }` |
| `escalate` | Stop autonomous control and hand off to a human. | — (see note) | `{ "root_id": uuid, "summary": string }` |

Notes on the four local actions:

- **`wait`** is first-class precisely so the supervisor does not spawn work
  while existing work is in flight. It blocks on the event cursor (or a timeout)
  and mutates nothing. `until` lists event kinds that should wake it early, for
  example `["task_finished"]`.
- **`request_input`** is the supervisor declaring that *it* cannot proceed
  without a human decision. It is not the agent's input channel. A task whose
  agent is blocked on a structured prompt is answered by the human-facing client
  with `agents.reply { "session_id": string, "request_id": string, "reply": … }`
  (where `reply` is an [input reply](#input-reply)); the supervisor observes
  `awaiting_input` and emits `request_input` to surface it.
- **`complete`** is a terminal decision. The daemon is not told; the supervisor
  simply stops issuing decisions for that root. Emit it only when
  `workflow.inspect` reports a terminal `state` and the outcome matches policy.
- **`escalate`** is also terminal. If policy requires stopping in-flight work
  before handing off, the supervisor emits a `cancel` decision first and then
  `escalate` once `workflow.inspect` reports `state == "cancelled"`.

### `input reply`

`agents.reply`'s `reply` field is tagged by `reply` and one of:

```json
{ "reply": "once" }
{ "reply": "always" }
{ "reply": "reject" }
{ "reply": "value", "value": "hunter2" }
{ "reply": "confirmed", "confirmed": true }
{ "reply": "cancelled" }
```

## The determinism boundary

The supervisor contract preserves the boundary stated in the
[architecture reference](./architecture): **the orchestrator never implements
its own agent loop.**

- favetto exposes a deterministic control plane. It validates params, enqueues
  work, cancels it, retries it, and emits events. It never chooses *which*
  action to take, never reads `reason`, and never stores supervisor policy.
- No LLM, planner, or reasoning loop is added to the daemon. All judgment lives
  in the external supervisor, which may be a shell script, a bespoke binary, a
  web UI, or another agent.
- The daemon keeps no per-supervisor session. A supervisor can disappear and a
  different one can take over from the same cursor; `workflow.inspect` plus
  `last_event_id` is the entire handoff state.
- `workflow.get` still returns the catalog graph; `workflow.inspect` returns the
  runtime graph. Neither replaces the other.

Because mutations go through the workflow RPCs, the same validation applies
regardless of who calls them. `workflow.spawn` rejects unknown catalog names and
out-of-root dependencies; `workflow.create` rejects duplicate keys, unknown
names, and cycles; `workflow.cancel` leaves already-terminal tasks untouched and
emits `task_cancelled` per task it stops, so no `needs` or join listener fires
for a cancelled root.

## Worked example

A supervisor starts `plan_issue` as its own root, then — once the plan lands —
spawns `implement_issue` as a child of that same root and waits for it.

1. **Observe.** On startup the supervisor has persisted `cursor = 4800`. It
   subscribes from that cursor and starts a root:

   ```json
   { "method": "events.subscribe", "params": { "last_event_id": 4800 } }
   { "method": "tasks.start", "params": { "name": "plan_issue" } }
   ```

   `tasks.start` replies with the task summary, whose `id` is the root id; a
   follow-up `workflow.inspect { "root_id": "<id>" }` shows `plan_issue`
   `running` and the other buckets empty.

2. **Wait** for the plan to finish.

   ```json
   { "action": "wait", "reason": "plan_issue is still running",
     "cursor": 4800, "params": { "until": ["task_finished"] } }
   ```

   The `task_finished` event for `plan_issue` arrives with `success: true`; the
   cursor advances to `4821`.

3. **Spawn** the implementer into the same root, deduped so a retry of this
   decision cannot double-start it:

   ```json
   { "method": "workflow.spawn",
     "params": { "root_id": "3f1c…", "name": "implement_issue",
                 "input": { "issue": "152" },
                 "dedupe_key": "implement:152" } }
   ```

4. **Inspect** again and **wait** until the root reaches a terminal state.

5. **Complete** once `workflow.inspect` reports `state == "succeeded"`:

   ```json
   { "action": "complete", "reason": "implemented and tests passed",
     "cursor": 4907, "params": { "root_id": "3f1c…", "summary": "152 merged" } }
   ```

   If instead the implementer ends `failed` with a `retryable` failure, the
   supervisor emits `retry`; a second failure with the policy exhausted becomes
   `escalate`.

This root had no catalog `needs` edges, so the supervisor drove the sequencing
itself. When the catalog does declare `needs`, the daemon starts the successor
automatically once the predecessor is terminal and `workflow.inspect` simply
reports the successor moving from `blocked` to `ready` to `running`; the
supervisor then only observes and decides `wait` or `complete`.

A minimal loop, in pseudocode:

```text
cursor = load_cursor()
subscribe(last_event_id = cursor)
loop:
    view = workflow.inspect(root_id)
    decision = decide(view, recent_events, policy)
    switch decision.action:
        inspect: view = workflow.inspect(root_id)
        spawn:   workflow.spawn(...) | workflow.create(...) | tasks.start(...)
        cancel:  workflow.cancel(root_id) | tasks.cancel(id)
        retry:   workflow.retry(task_id)
        wait:    block on the event cursor (or timeout)
        request_input: surface decision.prompt to the operator; pause
        complete: stop
        escalate: surface decision.summary to the operator; stop
    cursor = highest event id observed
    persist(cursor)
```

The supervisor — not the daemon — owns `decide`, `policy`, and the meaning of
"done". That is the whole point of the boundary.

## Reference implementation

The repository ships a runnable two-step supervisor so the contract can be read
alongside working code. It is an ordinary remote-API client — it links only the
wire types and the TUI's `Client`, never daemon code — and lives entirely outside
`crates/favetto`:

- **Engine** — `crates/favetto-tui/src/supervisor.rs`. `decide` is a pure
  function from a `WorkflowInspect` snapshot plus the policy to exactly one
  `Decision`; `run` applies each decision (`wait` sleeps, `spawn` calls
  `workflow.spawn`) until the root is terminal. It emits a subset of the closed
  vocabulary: `wait`, `spawn`, `complete`, `escalate`.
- **Binary** — `crates/favetto-tui/examples/reference_supervisor.rs`. It starts
  the first catalog task as its own root, waits for it, spawns the second into
  the same root, waits again, and completes.
- **Two-step catalog** — `tasks/examples/supervisor/plan.md` and
  `tasks/examples/supervisor/implement.md`. They deliberately declare **no**
  `spawn` or `needs` edge: the controller owns the sequencing, which is what
  keeps the daemon a deterministic executor.

Run it against a local daemon whose tasks directory contains the examples:

```bash
# terminal 1
favetto daemon --socket /tmp/favetto.sock

# terminal 2, from the repository root
cargo run -p favetto-tui --example reference_supervisor -- \
    --socket /tmp/favetto.sock \
    --first examples/supervisor/plan \
    --second examples/supervisor/implement
```

The `--remote`/`--token-file` flags attach over the authenticated WebSocket
instead of the Unix socket, and `--` `input`/`dedupe-key`/`wait-ms`/`max-cycles`
tune the run. `crates/favetto/tests/supervisor_e2e.rs` exercises the same path
end to end against the real daemon binary and the two catalog tasks, asserting
that the loop observes `wait`, submits `workflow.spawn`, and reaches
`complete`.
