# favetto

A long-running, LLM-driven agent orchestrator that automates software workflows
across **GitHub**, **Linear**, and **Gmail**. It is **MCP-native** for tooling and
**remote-TUI-first** from milestone 1.

```
┌───────────────────────────────────────────────────────────────┐
│                  TUI Client (ratatui)                         │
│   local attach  ─► unix:///tmp/favetto.sock              │
│   remote attach ─► ws://host:7878/rpc?token=…                 │
│   (same MessagePack wire protocol over both)                  │
└───────────────────────────────────────────────────────────────┘
                             │
┌───────────────────────────────────────────────────────────────┐
│                  Favetto Daemon                          │
│   Scheduler ─► Task Queue ─► Agent Runtime ─► MCP Client Pool │
│   Event Bus · Hook Engine · Notification Bus                  │
│   Integrations (GitHub/Linear/Gmail, thin reqwest/octocrab)   │
│   Persistence (SQLite/sqlx) · Remote API (axum)               │
└───────────────────────────────────────────────────────────────┘
```

## Design principles

- **MCP-first tooling** — every tool the agent calls is exposed via MCP (stdio
  today; streamable HTTP in later milestones). Internal tools and third-party MCP
  servers share one interface, so favetto is both a *consumer* and a
  *building block*.
- **Remote-TUI-first** — the TUI client and the daemon are decoupled over a single
  wire protocol; local attach is just a special case of remote attach.
- **Event-driven core** — everything is a task, an event, or a hook.
- **Plugin-style skills** — each task archetype is a folder with `AGENT.md` +
  `config.toml` + MCP server declarations.
- **Crash-resilient** — tasks and events persist across restarts; the event log is
  append-only and monotonic, doubling as the TUI's resume cursor.

## Workspace layout

```
crates/
  favetto-core/          shared wire protocol + domain types (no HTTP/DB/LLM)
  favetto/               single binary: daemon, tui, task-run, mcp-serve, ...
  favetto-integrations/  thin clients: Linear (GraphQL) + GitHub (octocrab)
  mcp-filesystem/             first-party MCP server (read/write/list under a root)
  mcp-linear/                 first-party MCP server (Linear issues/teams)
  mcp-github/                 first-party MCP server (GitHub issues/PRs)
  mcp-gmail/                  first-party MCP server (Gmail messages/threads/labels)
  mock-linear/                tiny GraphQL mock of Linear for offline dev
  mock-gmail/                 tiny Gmail REST mock for offline dev
skills/                       plugin-style skill folders (AGENT.md + config.toml)
workdir/                      sample filesystem target for the M2 demo
dev/Dockerfile                multi-stage build for mock-linear (docker-compose)
docker-compose.yml            local dev: mock-linear on :4000
hooks.toml                    example hooks (event + filter → action)
```

## Getting started

```bash
cargo build

# terminal 1 — the daemon (synthetic events drive the UI until M3+ integrations)
./target/debug/favetto daemon

# terminal 2 — the TUI (local attach over the unix socket)
./target/debug/favetto tui

# or attach remotely over WebSocket (token auto-created in ~/.local/share/favetto/token)
./target/debug/favetto tui --remote ws://127.0.0.1:7878 \
    --token-file ~/.local/share/favetto/token

# run a skill end-to-end through the MCP tool registry (M2 demo)
./target/debug/favetto task-run summarize_workspace --skills-dir skills
```

## Skills

A skill is a folder under `skills/`:

```toml
# skills/summarize_workspace/config.toml
[agent]
model = "mock"              # or "openai:<model>" (requires --features openai)
max_iterations = 10

[[agent.steps]]             # scripted steps consumed by the mock backend
kind = "tool"
server = "filesystem"
tool = "list_dir"
args = { path = "." }

[mcp.filesystem]            # MCP server this skill may connect to
transport = "stdio"
command = "mcp-filesystem"
args = ["--root", "./workdir"]

[tools.allow]               # per-skill tool allowlist (deny-by-default)
filesystem = ["list_dir", "read_file", "write_file"]
```

`AGENT.md` carries the role/prompt; the runtime injects it plus the allowlisted
tool schemas and drives a tool-calling loop through the MCP `ToolRegistry`.

## Milestones

| Milestone | Scope | Status |
|-----------|-------|--------|
| **M1** | Skeleton + remote TUI: daemon, SQLite, event bus, MessagePack wire protocol over Unix socket + WebSocket, token auth, ratatui client, synthetic events | ✅ Done |
| **M2** | MCP foundation + agent runtime: `rmcp` client, ToolRegistry, skill loader, LLM loop (mock + optional OpenAI), first-party `mcp-filesystem`, one end-to-end skill | ✅ Done |
| **M3** | GitHub + Linear integrations as first-party MCP servers, webhook receivers, hooks, local dev Linear via docker-compose, `implement_*` skills | ✅ Done |
| **M4** | Gmail: thin `reqwest` client + OAuth refresh + `mcp-gmail` + email summarizer skill | ✅ Done |
| **M5** | Scheduler + notifications: cron, task queue → runtime, notification channels, Scheduler + Notifications TUI tabs | ✅ Done |
| **M6** | Hardening: sandboxing, metrics, mTLS/pairing, favetto-as-MCP-server, replay/recovery tests | ⬜ Planned |

### M1 (done)

- Single binary `favetto` with `daemon`, `tui`, `task-run`, `mcp-serve`,
  `pair`, `token-rotate`.
- `favetto-core`: domain model, JSON-RPC-style MessagePack `Frame`, and the
  length-prefixed `FrameCodec` used over raw byte streams.
- Daemon: SQLite (`tasks` + append-only `events`), in-process event bus, axum
  WebSocket server (bearer token) and Unix socket server sharing one
  `serve_connection`, a synthetic event driver.
- TUI: Tasks, Chat, and Events tabs, connection state in the status bar,
  exponential backoff reconnect, resumable `events.subscribe { last_event_id }`.

### M2 (done)

- `mcp-filesystem`: a first-party stdio MCP server (rmcp server SDK) exposing
  `list_dir` / `read_file` / `write_file`, root-constrained.
- `mcp_client.rs`: rmcp client session that spawns a stdio child, initializes,
  and lists/calls tools. (Streamable-HTTP transport is declared in the config
  schema but deferred to a later milestone.)
- `tool_registry.rs`: aggregates tools with `(server_id, tool)` provenance and
  enforces the per-skill allowlist.
- `skills.rs` + `runtime.rs`: skill loader and a model-agnostic tool-calling loop.
  `model = "mock"` runs scripted steps; `--features openai` enables a thin-reqwest
  OpenAI backend.
- Verified end-to-end: `task-run summarize_workspace` drives the mock agent
  through the filesystem MCP server and writes `workdir/SUMMARY.md`.

### M3 (done)

- **`favetto-integrations`**: a shared crate with two thin clients —
  `linear` (minimal GraphQL over `reqwest`, typed structs for the used fields) and
  `github` (a thin `octocrab` wrapper with a base-URL override for offline dev).
- **`mcp-linear`** / **`mcp-github`**: first-party stdio MCP servers exposing the
  integrations as MCP tools (`list_teams`, `create_issue`, `comment_issue`, …
  and `list_issues`, `create_issue`, `comment_issue`, `open_pr`). Any MCP client
  can consume them.
- **Webhook receivers** (daemon, axum): `POST /webhooks/github` verifies
  `X-Hub-Signature-256` (HMAC-SHA256), `POST /webhooks/linear` verifies
  `Linear-Signature`; both persist + broadcast an event on a valid signature.
- **Hook engine**: `hooks.toml` maps `event` + optional `filter` → action
  (`run_skill` enqueues a task, `emit_event` derives an event, `notify` is M5).
  Hooks subscribe to the event bus and run on every matching event.
- **Local dev Linear**: `mock-linear` implements the tiny Linear GraphQL subset
  favetto uses; `docker compose up -d` runs it on `:4000`.
- Verified: `task-run implement_linear_ticket` drives the mock Linear through
  `list_teams → create_issue → comment_issue`; signed Linear/GitHub webhooks emit
  `ticket_created`/`issue_created` events, and the matching hooks enqueue a task
  and derive a `task_created` event.

### M4 (done)

- **`favetto-integrations::gmail`**: a thin `reqwest` client over the Gmail REST API
  (no `google-gmail1`). Typed structs for the used fields, `format=metadata` for
  list views, `format=full` only on demand, base64url MIME decode for incoming
  bodies, and raw-MIME build/encode for sending.
- **Auth**: a static token (`GMAIL_ACCESS_TOKEN`) or an OAuth 2.0 refresh flow
  (`GMAIL_CLIENT_ID`/`GMAIL_CLIENT_SECRET` + a refresh token from
  `GMAIL_REFRESH_TOKEN` or the OS keyring), with an in-memory access-token cache
  and expiry-aware refresh.
- **`mcp-gmail`**: first-party MCP server exposing `list_labels`, `list_messages`,
  `get_message`, `get_thread`, `send_message`, `modify_labels`.
- **Local dev Gmail**: `mock-gmail` implements the Gmail endpoints favetto uses;
  `docker compose up -d` runs it on `:4001`.
- Verified: `task-run summarize_email_thread` drives the mock Gmail through
  `list_messages → get_message` (decoding the body) and returns a summary.

### M5 (done)

- **Scheduler**: `tokio-cron-scheduler` runs cron schedules persisted in the
  `schedules` table. Each fire emits a `CronTick` event and enqueues a `run_skill`
  task. Schedules are managed live via `schedules.list` / `schedules.upsert` /
  `schedules.delete`.
- **Task executor**: a background worker drains the pending task queue and runs each
  task through the agent runtime (skill + MCP registry + LLM loop), closing the loop
  opened in M3 (hooks and schedules enqueue tasks that now actually execute).
- **Notifications**: pluggable channels (`log`, `webhook`, `ntfy`) with a persisted
  `notifications` history; hooks' `notify` action and `notifications.test` both send
  through it. Desktop (`notify-rust`) and email (`lettre`) channels are deferred.
- **TUI**: the Scheduler and Notifications tabs are now live.
- Verified: a `*/2 * * * * *` schedule fired repeatedly — each fire emitted
  `cron_tick`, enqueued a task, and the executor ran it to `succeeded` (emitting
  `task_completed`); `log` and `webhook` notifications delivered and were persisted.

## Webhooks & hooks

```bash
# run the daemon with webhook secrets + hooks
GITHUB_WEBHOOK_SECRET=… LINEAR_WEBHOOK_SECRET=… \
  ./target/debug/favetto daemon --hooks hooks.toml
```

GitHub signs with `X-Hub-Signature-256`; Linear signs with `Linear-Signature`.
Both are HMAC-SHA256 over the raw body, verified in constant time. Hooks run
against every persisted event; `run_skill` enqueues a task (executed by the task
executor), `emit_event` derives a new event, and `notify` sends a notification
through a channel (e.g. `action = { type = "notify", channel = "webhook",
config = { url = "http://…" } }`).

## Configuration & credentials

favetto reads a global config file (parsed with the `config` crate, located via
the `dirs` crate) at **`~/.config/favetto/config.toml`** — override with
`--config <path>` or `$FAVETTO_CONFIG`. It declares LLM providers, global MCP
servers, and daemon defaults:

```toml
# ~/.config/favetto/config.toml
[agent]
model = "echo"                 # default model for chat; "openai:gpt-4o" for OpenAI

[providers.openai]             # LLM providers
kind = "openai"
api_key_env = "OPENAI_API_KEY" # or api_key = "..."
model = "gpt-4o"
base_url = "https://api.openai.com/v1"   # optional

[mcp.filesystem]               # global MCP servers (skills can rely on these)
transport = "stdio"
command = "mcp-filesystem"
args = ["--root", "./workdir"]

[daemon]                       # defaults, overridable by CLI flags
listen = "127.0.0.1:7878"
socket = "/tmp/favetto.sock"
skills_dir = "skills"
```

Data (SQLite + bearer token) lives in **`~/.local/share/favetto`** (or
`$FAVETTO_DATA_DIR`). Settings resolve in the order CLI flag → config file →
built-in default. See [`config.example.toml`](config.example.toml) for a full,
annotated example.

Credentials are read from the environment (OS keyring integration arrives with
Gmail in M4). The M3 integrations use:

- `GITHUB_TOKEN` — personal access token for `mcp-github` and GitHub webhooks
  (`GITHUB_WEBHOOK_SECRET` for HMAC verification).
- `LINEAR_API_KEY` / `LINEAR_BASE_URL` — for `mcp-linear` (`LINEAR_WEBHOOK_SECRET`
  for webhook verification). `LINEAR_BASE_URL` can point at the local dev
  instance described below.
- `GMAIL_ACCESS_TOKEN` (static) or `GMAIL_CLIENT_ID` + `GMAIL_CLIENT_SECRET` +
  `GMAIL_REFRESH_TOKEN` (OAuth refresh) — for `mcp-gmail` (`GMAIL_BASE_URL` points
  at the local dev instance).

## Local dev (docker-compose)

Local mocks of Linear's GraphQL API and Gmail's REST API are provided for offline
development. They implement the small subsets of each schema favetto uses.

```bash
docker compose up -d            # mock-linear on :4000, mock-gmail on :4001

LINEAR_API_KEY=dev LINEAR_BASE_URL=http://localhost:4000/graphql \
  ./target/debug/favetto task-run implement_linear_ticket --skills-dir skills

GMAIL_ACCESS_TOKEN=dev GMAIL_BASE_URL=http://localhost:4001/gmail/v1 \
  ./target/debug/favetto task-run summarize_email_thread --skills-dir skills

docker compose down
```

## Chat (per-task LLM conversation)

From the TUI, highlight a task in the **Tasks** tab (↑/↓) and press **Enter** to
open the **Chat** tab: it displays the agent conversation seeded from the task's
skill prompt and input. Type a message and press **Enter** to send it — the
daemon runs one agent turn and appends the reply.

The chat uses the same `ModelBackend` abstraction as skills: it falls back to an
echo backend offline, or uses `openai:<model>` when `--features openai` and
`OPENAI_API_KEY` are set. Chat sessions are in-memory for now; they'll persist
once the scheduler lands in M5.

## Remote API (M1)

The daemon exposes one wire protocol over two transports:

- **Unix socket** `/tmp/favetto.sock` (local, trusted).
- **WebSocket** `ws://127.0.0.1:7878/rpc` (bearer token required).

Methods: `system.ping`, `tasks.list`, `tasks.cancel`, `events.tail`,
`events.subscribe`, `chat.open`, `chat.send`, `chat.messages`,
`schedules.list`, `schedules.upsert`, `schedules.delete`, `notifications.list`,
`notifications.test`. Server pushes: `event`, `task.updated`, `log.line`.
