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

## Server → client pushes

| Method | Purpose |
|--------|---------|
| `event` | A persisted event. |
| `task.updated` | A task row changed. |
| `catalog.updated` | The task catalog changed on disk; clients should re-fetch it. |
| `agent.output` | Raw PTY output (base64) from a running agent session. |
| `agent.exit` | An agent session's child process exited. |
| `agent.state` | A session's folded live state (activity/usage) changed. |

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

