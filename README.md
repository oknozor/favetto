# favetto

A long-running, LLM-driven agent orchestrator with a **remote-TUI-first** design.
It schedules and runs declarative tasks by delegating to external coding-agent CLIs
(opencode, Claude Code, pi, Mistral Vibe, …), embedded as live terminals — the
orchestrator never implements its own agent loop.

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
│   Scheduler ─► Task Queue ─► Agent Sessions (PTY) ─► Agent CLIs│
│   Event Bus · Hook Engine · Notification Bus                  │
│   Persistence (SQLite/sqlx) · Remote API (axum)               │
└───────────────────────────────────────────────────────────────┘
```

## Design principles

- **Agents as workers** — favetto orchestrates; the coding loop is an external
  agent CLI embedded as a live PTY session that can be attached, reattached, and
  keystroke-driven from the TUI.
- **Remote-TUI-first** — the TUI client and the daemon are decoupled over a single
  wire protocol; local attach is just a special case of remote attach.
- **Event-driven core** — everything is a task, an event, or a hook.
- **Declarative tasks** — each task is a Markdown file with a TOML header
  (`agent`, plus optional schedule/dependency) and a prompt body.
- **Crash-resilient** — tasks and events persist across restarts; the event log is
  append-only and monotonic, doubling as the TUI's resume cursor.

## Workspace layout

```
crates/
  favetto-core/          shared wire protocol + domain types (no HTTP/DB/LLM)
  favetto-providers/     provider/model catalog (opencode auth + models.dev)
  favetto/               single binary: daemon, tui, agent exec wrapper
tasks/                        task catalog: `*.md` files with TOML header + prompt
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

# configure external agents, then start tasks from the TUI
```

## Tasks (catalog)

A task is a Markdown file under `tasks/` with a TOML header and the prompt as the
body:

```md
agent = "opencode"                      # optional — run through this external agent
provider = "jev"                        # optional — model provider
model = "1.13"                          # optional — model id (uses the agent's run_args)
cwd = "/code/che"                       # optional — repo/checkout the agent works in
schedule = "0 8 * * * *"                # optional — makes this a recurring task
needs = "another_task:finished"         # optional — start when `another_task` ends
---

You are an engineering agent that implements Linear tickets end-to-end.
…
```

- `agent` names an `[agents.*]` entry (see below). When set — or when
  `[agent].default` is configured — the task runs through that agent CLI with the
  task prompt (headless for catalog runs, interactive in the Agent tab). A task
  with no agent and no default fails to start.
- `provider` and `model` select the model per task. When `model` is set the agent's
  `run_args` template is used (`{provider}`/`{model}` substituted); otherwise the
  agent's `headless_args` are used and the agent's own default model applies. A
  `provider` without a `model` is ignored.
- `cwd` is the directory the agent runs in (e.g. a repo checkout); it takes
  precedence over the agent's configured `cwd`. Per-run `tasks.start` `input.cwd`
  wins over both.
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
| **M2** | External agent sessions: PTY spawn, server-side `vt100` emulation, remote attach/reattach, terminal query replies | ✅ Done |
| **M3** | Scheduler + notifications: cron, task queue → agent sessions, notification channels, Scheduler + Notifications TUI tabs | ✅ Done |
| **M4** | Hardening: metrics, pairing, webhook receivers + hooks, parallel git-worktree execution | ✅ Done |

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

## Configuration

favetto reads a global config file (parsed with the `config` crate, located via
the `dirs` crate) at **`~/.config/favetto/config.toml`** — override with
`--config <path>` or `$FAVETTO_CONFIG`. It declares the external agents and daemon
defaults:

```toml
# ~/.config/favetto/config.toml
[agent]
default = "opencode"           # default external agent (a key in [agents.*])

[agents.opencode]              # external coding agents
command = "opencode"
args = []
prompt_args = ["--prompt", "{prompt}"]   # seed an interactive session
submit_prompt = true                     # press Enter so the seeded prompt is sent
headless_args = ["run", "--auto", "{prompt}"]   # unattended runs without a model
run_args = ["run", "--model", "{provider}/{model}", "--auto", "--format", "json", "{prompt}"]
resume_args = ["--session", "{session_id}"]     # reopen a run's session
session_id_json_key = "sessionID"               # captured from run JSON output

[daemon]                       # defaults, overridable by CLI flags
listen = "127.0.0.1:7878"
socket = "/tmp/favetto.sock"
tasks_dir = "tasks"
```

Data (SQLite + bearer token) lives in **`~/.local/share/favetto`** (or
`$FAVETTO_DATA_DIR`). Settings resolve in the order CLI flag → config file →
built-in default. See [`config.example.toml`](config.example.toml) for a full,
annotated example.

Webhook receivers read their signing secrets from the environment:
`GITHUB_WEBHOOK_SECRET` and `LINEAR_WEBHOOK_SECRET`.

## Embedded agents

favetto does not reimplement a coding agent: it runs a configured agent CLI
(opencode, Claude Code, pi, Mistral Vibe, …) in a PTY on the daemon and embeds
its live terminal in the TUI's **Agent** tab.

```toml
# ~/.config/favetto/config.toml
[agent]
default = "opencode"

[agents.opencode]
command = "opencode"
args = []
prompt_args = ["--prompt", "{prompt}"]   # seed an interactive session with the task prompt
submit_prompt = true                     # press Enter so the seeded prompt is sent
headless_args = ["run", "--auto", "{prompt}"]   # unattended runs without a model
run_args = ["run", "--model", "{provider}/{model}", "--auto", "--format", "json", "{prompt}"]
resume_args = ["--session", "{session_id}"]     # reopen a session in the TUI
session_id_json_key = "sessionID"               # captured from the run's JSON events
```

Model selection lives in the invocation templates, because many CLIs (e.g.
opencode) only accept `--model` on a subcommand such as `run`. When a task sets
`model`, favetto uses `run_args`; otherwise `headless_args` (agent default model).
`{provider}` and `{model}` are the task's separate provider/model values. To also
reattach to a finished run, set `resume_args` and `session_id_json_key`: the run's
line-delimited JSON is scanned for the session id (opencode's `run --format json`
puts `sessionID` on every event), it is stored on the task, and opening that task
launches `resume_args`. An agent without these fields keeps the simpler
PTY-replay behavior.

A task may set `provider = "jev"` and `model = "1.13"` to pick the model per task;
a `provider` without a `model` is ignored and the agent default applies.

From the **Tasks** tab, highlight a task and press **Enter** to open its agent
session, seeded with the task prompt and running in the task's workspace. Opening
a task first reattaches to its running session; if the run already finished and a
session id was captured, it reopens that session with `resume_args`. Press
**Ctrl+N** in the Agent tab to force a new session. The daemon keeps a `vt100`
emulator per session and streams self-contained full-screen frames; the panel
parses and renders them, keys are forwarded to the agent, and the PTY is resized
to fit. **Ctrl+Q** detaches without stopping the session (it keeps running on the
daemon). Common terminal queries (cursor position, device attributes, colours,
mode reports) are answered by the daemon on the PTY.

**Keyboard focus.** When the Agent panel opens it captures the keyboard by default,
so the embedded agent receives every keystroke (including ones favetto would
otherwise use). Press **Ctrl+Y** to toggle focus between the agent and favetto; the
active owner is shown in the bottom status bar (`focus: agent` / `focus: favetto`).
With favetto focused, **Ctrl+Q** leaves the panel and **Ctrl+N** starts a new
session. Ctrl+Y is chosen not to clash with opencode's default keybinds.

**Mouse.** Clicks are delivered to both sides without conflict: favetto handles
clicks on its own regions (the tab bar) and switches tabs, while clicks in the
terminal area are forwarded to the agent as mouse reports whenever the agent has
enabled mouse reporting. This requires the daemon to stream `state_formatted`
frames, which carry the agent's input modes alongside the screen contents.

`prompt_args` seeds the prompt (`{prompt}` is substituted); some agents only
pre-fill their input, so set `submit_prompt = true` to send Enter once the UI has
settled. With no `prompt_args`, the prompt is written to the agent's stdin instead.

Agents are launched through a small supervisor (`favetto __agent-exec`) that owns
a process group and dies with the daemon — on both `SIGTERM` and `SIGKILL` — taking
the agent and its children with it. Live PTYs therefore do **not** survive a daemon
restart, but an agent's own session (captured via `session_id_json_key`) does, so
tasks can be resumed after a restart.

Catalog tasks run through their `agent` (or `[agent].default`) in headless mode
using `headless_args` (or `run_args` when a model is set); a task with no agent
(and no default) fails to start. An external agent **without** the relevant args
is rejected rather than launched interactively (an interactive TUI would never
exit and would hang the task).

## Parallel execution & git worktrees

The executor is serial by default. Parallelism and isolation are configurable:

```toml
[executor]
parallel = true          # default false
max_concurrency = 4
worktree = true          # default true; used only when `parallel`
# worktree_dir = "~/.local/share/favetto/worktrees"  # absolute or repo-relative
keep_worktree = true     # default true; false removes the worktree afterwards
```

- **Parallel + git repository** → each task runs in its own `git worktree`
  (branch `favetto/<task>-<id>`, path under the worktree dir), so tasks don't
  step on each other. Worktrees are kept by default so the agent's branch and
  changes can be inspected.
- **Parallel + not a repository** → tasks that share a working directory are
  serialized (one at a time); different directories still run concurrently.
- **Not parallel** → strict global serialization, as before.

`worktree` is ignored unless `parallel = true`. The effective directory is
`input.cwd` → task `cwd` → the daemon's working directory.

## Ctrl+P menu

Press **Ctrl+P** in the TUI to open a floating menu with guided, step-by-step
forms for:

- **New one-shot task** — a wizard (agent → provider → model → directory) that
  starts an inline, interactive task and opens it in the Agent panel for you to
  prompt. It emits the normal task events but is **not** written to the catalog.
- **Add task to catalog** — writes a task `.md` file (does not run it).
- **Create a schedule event** — adds a cron schedule.
- **Create a notification (hook)** — adds a hook that reacts to an event kind and
  sends a notification through a channel.

### One-shot tasks

The wizard lists agents from the config, then providers and models from the
`favetto-providers` catalog: providers are the ones authenticated with opencode
(read from `~/.local/share/opencode/auth.json`) and their models come from the
models.dev catalog (`https://models.dev/api.json`, overridable with
`OPENCODE_MODELS_URL`). The daemon caches the catalog for its lifetime. The
directory step is prefilled with the daemon's working directory. On completion
favetto creates a task row (input carries the inline definition, no `.md` file),
emits `task_idle`/`task_started`, and opens an interactive session using the
agent's `interactive_model_args` (so a model can be applied even when the agent's
main TUI has no model flag — for opencode, `mini --model provider/model`). The task
is marked completed/failed when that session exits.

## Remote API

The daemon exposes one wire protocol over two transports:

- **Unix socket** `/tmp/favetto.sock` (local, trusted).
- **WebSocket** `ws://127.0.0.1:7878/rpc` (bearer token required).

Methods: `system.ping`, `tasks.list`, `tasks.start`, `tasks.cancel`,
`events.tail`, `events.subscribe`, `agents.list`, `agents.start`, `agents.input`,
`agents.resize`, `agents.attach`, `agents.close`, `schedules.list`,
`schedules.upsert`, `schedules.delete`, `notifications.list`,
`notifications.test`, `hooks.upsert`. Server pushes:
`event`, `task.updated`, `log.line`, `agent.output`, `agent.exit`.

The daemon also serves HTTP endpoints: `GET /metrics` (Prometheus),
`POST /pair/generate` and `POST /pair/exchange` (pairing), plus the webhook
receivers.
