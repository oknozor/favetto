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
- **Declarative tasks** — each task is a Markdown file with a TOML header (model,
  optional schedule/dependency) and a prompt body.
- **Crash-resilient** — tasks and events persist across restarts; the event log is
  append-only and monotonic, doubling as the TUI's resume cursor.

## Workspace layout

```
crates/
  favetto-core/          shared wire protocol + domain types (no HTTP/DB/LLM)
  favetto/               single binary: daemon, tui, mcp-serve, ...
  favetto-integrations/  thin clients: Linear (GraphQL) + GitHub (octocrab)
  mcp-filesystem/             first-party MCP server (read/write/list under a root)
  mcp-linear/                 first-party MCP server (Linear issues/teams)
  mcp-github/                 first-party MCP server (GitHub issues/PRs)
  mcp-gmail/                  first-party MCP server (Gmail messages/threads/labels)
  mock-linear/                tiny GraphQL mock of Linear for offline dev
  mock-gmail/                 tiny Gmail REST mock for offline dev
tasks/                        task catalog: `*.md` files with TOML header + prompt
workdir/                      sample filesystem target for the M2 demo
dev/Dockerfile                multi-stage build for mock-linear (docker-compose)
docker-compose.yml            local dev: mock-linear on :4000
hooks.toml                    example hooks (event + filter → action)
```

## Getting started

```bash
cargo build

# terminal 1 — the daemon
./target/debug/favetto daemon

# terminal 2 — the TUI (local attach over the unix socket)
./target/debug/favetto tui

# or attach remotely over WebSocket (token auto-created in ~/.local/share/favetto/token)
./target/debug/favetto tui --remote ws://127.0.0.1:7878 \
    --token-file ~/.local/share/favetto/token

# configure providers + MCP servers, then start tasks from the TUI
```

## Tasks (catalog)

A task is a Markdown file under `tasks/` with a TOML header and the prompt as the
body:

```md
model = "deepseek:deepseek-v4-flash"   # required — the LLM provider/model
schedule = "0 8 * * * *"               # optional — makes this a recurring task
needs = "another_task:finished"        # optional — start when `another_task` ends
---

You are an engineering agent that implements Linear tickets end-to-end.
…
```

- `model` selects the provider (any `[providers.*]` entry), so each task talks to
  its own model.
- `schedule` registers a recurring cron task.
- `needs` declares a dependency: this task auto-starts when the named task emits
  its `finished` event (mutually exclusive with `schedule`).

The daemon loads the catalog from `--tasks-dir` (default `tasks/`). From the TUI,
the **Catalog** tab lists the catalog and Enter starts a task; the Ctrl+P menu's
"Add task" writes a new `.md` file (it does not run it). Task lifecycle emits
`task_idle` / `task_started` / `task_finished` events.

## Milestones

| Milestone | Scope | Status |
|-----------|-------|--------|
| **M1** | Skeleton + remote TUI: daemon, SQLite, event bus, MessagePack wire protocol over Unix socket + WebSocket, token auth, ratatui client | ✅ Done |
| **M2** | MCP foundation: `rmcp` client, ToolRegistry, agent runtime, first-party `mcp-filesystem` | ✅ Done |
| **M3** | GitHub + Linear integrations as first-party MCP servers, webhook receivers, hooks, local dev mocks via docker-compose | ✅ Done |
| **M4** | Gmail: thin `reqwest` client + OAuth refresh + `mcp-gmail` | ✅ Done |
| **M5** | Scheduler + notifications: cron, task queue → runtime, notification channels, Scheduler + Notifications TUI tabs | ✅ Done |
| **M6** | Hardening: orchestrator-as-MCP-server (`mcp-serve`), metrics, pairing, sandbox + replay tests | ✅ Done |

## Webhooks & hooks

```bash
# run the daemon with webhook secrets + hooks
GITHUB_WEBHOOK_SECRET=… LINEAR_WEBHOOK_SECRET=… \
  ./target/debug/favetto daemon --hooks hooks.toml
```

GitHub signs with `X-Hub-Signature-256`; Linear signs with `Linear-Signature`.
Both are HMAC-SHA256 over the raw body, verified in constant time. Hooks run
against every persisted event; `run_task` enqueues a task (executed by the task
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
model = "deepseek:deepseek-v4-flash"   # "<provider>:<model>"; "echo" for offline

[providers.deepseek]           # any OpenAI-compatible provider
kind = "openai"
api_key_env = "DEEPSEEK_API_KEY"   # or api_key = "..."
model = "deepseek-v4-flash"
base_url = "https://api.deepseek.com"

[providers.openai]             # another provider
kind = "openai"
api_key_env = "OPENAI_API_KEY"
model = "gpt-4o"
base_url = "https://api.openai.com/v1"   # optional

[mcp.filesystem]               # global MCP servers (task prompts can rely on these)
transport = "stdio"
command = "mcp-filesystem"
args = ["--root", "./workdir"]

[daemon]                       # defaults, overridable by CLI flags
listen = "127.0.0.1:7878"
socket = "/tmp/favetto.sock"
tasks_dir = "tasks"
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
  # start `implement_linear_ticket` from the TUI Catalog tab (or via the API)

GMAIL_ACCESS_TOKEN=dev GMAIL_BASE_URL=http://localhost:4001/gmail/v1 \
  # start `summarize_email_thread` from the TUI Catalog tab (or via the API)

docker compose down
```

## Chat (per-task LLM conversation)

From the TUI, highlight a task in the **Tasks** tab (↑/↓) and press **Enter** to
open the **Chat** tab: it displays the agent conversation seeded from the task's
task prompt and input. Type a message and press **Enter** to send it — the
daemon runs one agent turn and appends the reply.

The chat uses the same `ModelBackend` abstraction as tasks: it resolves the
global `[agent]` model through the configured `[providers]` (OpenAI-compatible —
e.g. `deepseek:deepseek-v4-flash`), falling back to a local echo backend when no
provider/key is available. Chat sessions are in-memory for now; they'll persist
once the scheduler lands in M5.

## Ctrl+P menu

Press **Ctrl+P** in the TUI to open a floating menu with guided, step-by-step
forms for:

- **Add provider / model** — registers an LLM provider (persisted to the config
  file and applied live).
- **Add task to catalog** — writes a task `.md` file (does not run it).
- **Create a schedule event** — adds a cron schedule.
- **Create a notification (hook)** — adds a hook that reacts to an event kind and
  sends a notification through a channel.

## Remote API

The daemon exposes one wire protocol over two transports:

- **Unix socket** `/tmp/favetto.sock` (local, trusted).
- **WebSocket** `ws://127.0.0.1:7878/rpc` (bearer token required).

Methods: `system.ping`, `tasks.list`, `tasks.start`, `tasks.cancel`,
`events.tail`, `events.subscribe`, `chat.open`, `chat.send`, `chat.messages`,
`schedules.list`, `schedules.upsert`, `schedules.delete`, `notifications.list`,
`notifications.test`, `config.set_provider`, `hooks.upsert`. Server pushes:
`event`, `task.updated`, `log.line`.

The daemon also serves HTTP endpoints: `GET /metrics` (Prometheus),
`POST /pair/generate` and `POST /pair/exchange` (pairing), plus the webhook
receivers. And `favetto mcp-serve` exposes the whole thing as an MCP server.
