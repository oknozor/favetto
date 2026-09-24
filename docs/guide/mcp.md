# MCP supervisor

`favetto-mcp` is a [Model Context Protocol](https://modelcontextprotocol.io)
(MCP) server that exposes a favetto daemon to an MCP-capable agent — Claude
Desktop, an IDE assistant, a custom client — so the model can **supervise a
workflow**. It is an ordinary remote-API client, exactly like the TUI: it links
only the shared wire types and the `favetto-tui` client, never daemon code.

The server holds **no policy**. favetto stays a deterministic executor; the MCP
client observes the workflow, picks the next action from a closed vocabulary, and
the daemon executes it. See the
[supervisor contract](../reference/supervisor-contract) for the full contract.

## Run

Start a daemon, then point the server at its socket:

```bash
# terminal 1
favetto daemon --socket /tmp/favetto.sock

# terminal 2
favetto-mcp --socket /tmp/favetto.sock
```

The server speaks newline-delimited JSON-RPC 2.0 on **stdio**; it logs to
**stderr**, so stdout is safe to use as the protocol channel. For a remote
daemon, use the same flags as the TUI:

```bash
favetto-mcp --remote ws://HOST:7878 --token-file ~/.local/share/favetto/token
```

`$FAVETTO_URL` is honoured when `--remote` is omitted, and the default socket is
`/tmp/favetto.sock`. `--log` sets the stderr tracing filter (default `info`).

### Register with an MCP client

```json
{
  "mcpServers": {
    "favetto": {
      "command": "favetto-mcp",
      "args": ["--socket", "/tmp/favetto.sock"]
    }
  }
}
```

## Protocol

The server implements the `2025-06-18` MCP lifecycle over stdio. It echoes a
client's requested revision when it is one of `2025-03-26`, `2025-06-18`, or
`2025-11-25`, and otherwise replies with `2025-06-18`. The stateless
`2026-07-28` revision is not implemented yet.

Only the subset needed to supervise is served: `initialize`, `ping`,
`tools/list` + `tools/call`, `resources/list` + `resources/templates/list` +
`resources/read`, and `prompts/list` + `prompts/get`.

## Tools

Every tool is a typed projection of one allowed RPC. There is no generic
passthrough, and mutations are limited to the supervisor vocabulary.

| Tool | RPC | Arguments |
|------|-----|-----------|
| `favetto_inspect` | `workflow.inspect` | `{ root_id }` |
| `favetto_start_task` | `tasks.start` | `{ name, input? }` |
| `favetto_spawn` | `workflow.spawn` | `{ root_id?, name, input?, depends_on?, dedupe_key? }` |
| `favetto_create_workflow` | `workflow.create` | `{ idempotency_key, root_id?, tasks: [{ key, name, input?, depends_on? }] }` |
| `favetto_cancel_workflow` | `workflow.cancel` | `{ root_id }` |
| `favetto_cancel_task` | `tasks.cancel` | `{ id }` |
| `favetto_retry_task` | `workflow.retry` | `{ task_id }` |
| `favetto_get_task` | `tasks.get` | `{ id }` |
| `favetto_list_tasks` | `tasks.list` | `{ limit? }` |
| `favetto_agents_list` | `agents.list` | `{}` |
| `favetto_agents_reply` | `agents.reply` | `{ session_id, request_id, reply, value?, confirmed? }` |
| `favetto_events_tail` | `events.tail` | `{ limit? }` |
| `favetto_wait` | — (local) | `{ until?, timeout_ms?, root_id? }` |

`favetto_wait` blocks on the event cursor (bounded: default 30s, hard cap 120s)
and returns the observed events, the advanced cursor, and `timed_out`. A
`timeout_ms` of `0` returns immediately without subscribing.

`favetto_agents_reply.reply` is a flat enum (`once`, `always`, `reject`, `value`,
`confirmed`, `cancelled`); `value` requires a `value`, and `confirmed` requires a
`confirmed`. The tool builds the tagged wire reply the daemon expects.

Bad arguments, an unknown tool, and an RPC failure all come back as a **tool
execution error** (`isError: true`) with a text explanation — never as a protocol
error — so the model can read the message and correct itself.

## Resources

| Resource | RPC |
|----------|-----|
| `favetto://events` (optional `?limit=N`) | `events.tail` |
| `favetto://catalog` | `catalog.list` |
| `favetto://workflow/{root_id}` | `workflow.inspect` |

Each resource reads as `application/json`.

## Prompt

`supervise_workflow` takes an optional `root_id` argument and returns the
one-decision-per-cycle contract: the eight actions (`inspect`, `spawn`, `cancel`,
`retry`, `wait`, `request_input`, `complete`, `escalate`), the action → tool
mapping, and the rule to re-inspect between mutations.

## Boundary

The server deliberately exposes **no raw PTY access** and no administration
surface. `agents.reply` is the only agent-input tool; `agents.input`,
`agents.start`, `agents.resize`, `agents.attach`, `agents.close`,
`tasks.start_oneshot`, `catalog.add`, `catalog.update`, `schedules.*`,
`notifications.*`, and `hooks.upsert` are not available. The daemon and
`favetto-core` are unchanged: this is another external supervisor, like the
reference example in the [supervisor contract](../reference/supervisor-contract).
