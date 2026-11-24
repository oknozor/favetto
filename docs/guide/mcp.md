# MCP

Favetto uses MCP in two independent directions:

- **Favetto as an MCP server** — `favetto-mcp` exposes a daemon to an MCP client
  so the model can supervise a workflow (this page's main subject).
- **Favetto as an ACP client** — the daemon passes user-owned MCP servers to an
  ACP agent through `mcpServers`. See [MCP servers for agents](#mcp-servers-for-agents)
  below.

## MCP supervisor

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
| `favetto_start_task` | `tasks.start` | `{ name, input?, agent?, provider?, model? }` |
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

`favetto://catalog` lists repo-local tasks too, under their qualified
`repo:<name>/<path>` identity, and `favetto_start_task` accepts those names
unchanged (see [Repository-local tasks](./catalog#repository-local-tasks)).

## Prompt

`supervise_workflow` takes an optional `root_id` argument and returns the
one-decision-per-cycle contract: the eight actions (`inspect`, `spawn`, `cancel`,
`retry`, `wait`, `request_input`, `complete`, `escalate`), the action → tool
mapping, and the rule to re-inspect between mutations.

## Boundary

The server deliberately exposes **no raw PTY access** and no administration
surface. `agents.reply` is the only agent-input tool; `agents.input`,
`agents.start`, `agents.resize`, `agents.attach`, `agents.close`,
`tasks.start_oneshot`, `catalog.add`, `catalog.update`, `schedules.*`, and
`notifications.*` are not available. The daemon and
`favetto-core` are unchanged: this is another external supervisor, like the
reference example in the [supervisor contract](../reference/supervisor-contract).

## MCP servers for agents

ACP v1 sessions accept an `mcpServers` list. Favetto forwards registered
definitions on `session/new`, and replays the same selection on `session/load`
and `session/resume`. It never installs or probes packages: you install the
server yourself and register its command or URL.

### Register a definition

The daemon persists definitions in SQLite. A `[[mcp.servers]]` config entry seeds
the registry at startup only; later app edits survive restarts.

```toml
[[mcp.servers]]
name = "files"
command = "npx"                       # resolved on PATH, stored absolute
args = ["-y", "@modelcontextprotocol/server-filesystem", "/work"]
enabled = true
approved = true                       # operator-authored seeds are pre-approved
env = { GITHUB_TOKEN = "..." }        # literal, stored in a 0600 secrets file

[[mcp.servers]]
name = "remote"
transport = "http"
url = "https://example.com/mcp"
headers = { Authorization = "Bearer ..." }
```

Prefer `env_from` / `headers_from` to keep the config free of secrets:

```toml
[mcp.servers.env_from]
GITHUB_TOKEN = "GITHUB_TOKEN"         # entry name -> daemon env var
```

The management RPCs are:

| Method | Purpose |
|---|---|
| `mcp.list` / `mcp.get` | Redacted definitions, sorted by name. |
| `mcp.add` / `mcp.update` | Register or replace. Changing the command/args fingerprint revokes approval unless `approve: true`. |
| `mcp.remove` | Delete the definition and its stored literals. |
| `mcp.set_enabled` | Enable/disable without deleting. |
| `mcp.approve` | Record or revoke stdio launch consent. |

Only stdio needs approval: enabling a command is not consent to run it with the
daemon's privileges. HTTP/SSE definitions are gated on the agent's advertised
`mcpCapabilities` instead.

### Secrets

Secret values are resolved in memory at launch. `env`/`header` declarations name
a source (`env`, `command`, or a write-only `literal`). RPC responses return only
the declaration and `has_value`; values are never logged, embedded in errors, or
included in session payloads. Literal values live in
`<data_dir>/mcp-secrets.json` with mode `0600`.

### Selection and fallback

- `agents.start` / `tasks.start_oneshot` take `mcp_servers: ["name"]`.
- A catalog task header can set `mcp = ["name"]`.
- Unset means every enabled + approved definition; an explicit empty list sends
  none.

The session's `mcp` field reports `selected`, `active`, and `warnings`. Removed,
disabled, unapproved, unresolvable or unsupported transports are omitted with an
actionable warning and the session still starts. An unknown name in an explicit
request is an `InvalidParams` error rather than a silent change.
