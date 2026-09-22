# Remote API

The daemon exposes one wire protocol over two transports:

- **Unix socket** `/tmp/favetto.sock` (local, trusted).
- **WebSocket** `ws://127.0.0.1:7878/rpc` (bearer token required).

Both carry the same MessagePack-encoded frames. A frame is a tagged envelope with
one of three shapes:

```rust
enum Frame {
    Request(Request),           // client → server
    Response(Response),         // server → client reply
    Notification(Notification), // unsolicited server push
}
```

A `Request` carries `id`, `method`, and `params`; the matching `Response` carries
the same `id` plus either `result` or a structured `error` (`code`, `message`,
optional `data`). Errors use the JSON-RPC codes plus `-32001` for an
unauthenticated/rejected token.

## Client → server methods

| Method | Purpose |
|--------|---------|
| `system.ping` | Liveness check. |
| `tasks.list` | List tasks. |
| `tasks.start` | Start a catalog task by name. |
| `tasks.start_oneshot` | Start a one-shot task from an inline definition (not added to the catalog) and open its interactive agent session. |
| `tasks.cancel` | Cancel a task. |
| `catalog.list` | List the task catalog. |
| `catalog.get` | Fetch a catalog task's raw Markdown source (for the preview pane). |
| `catalog.add` | Add a task definition to the catalog (does not run it). |
| `workflow.get` | Fetch the catalog workflow graph as Graphviz DOT (`dot` plus the persisted `path`). |
| `events.tail` | Tail persisted events. |
| `events.subscribe` | (Re)subscribe to the live event stream; accepts `last_event_id` to replay missed events before switching to live delivery. |
| `agents.list` | List configured external agents and live agent sessions. |
| `agents.start` | Start an external agent session (optionally attached to a task). |
| `agents.input` | Write raw bytes (base64) to a session's PTY. |
| `agents.resize` | Resize a session's PTY. |
| `agents.attach` | Attach to a session: returns the session and a replay of its output. |
| `agents.close` | Terminate a session. |
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
| `log.line` | A daemon log line. |
| `agent.output` | Raw PTY output (base64) from a running agent session. |
| `agent.exit` | An agent session's child process exited. |

## HTTP endpoints

The daemon also serves HTTP endpoints alongside the RPC transports:

- `GET /metrics` — Prometheus metrics.
- `POST /pair/generate` and `POST /pair/exchange` — pairing.
- The webhook receivers (`/webhooks/github`, and the Linear receiver) — see
  [Webhooks & hooks](../guide/webhooks).
