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

### Checks

CI runs the same three gates locally:

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo nextest run --locked --workspace   # install: cargo install cargo-nextest
```

**Appearance.** The TUI ships a built-in dark and light style that is selected
automatically from the terminal background (an OSC 11 query, then `COLORFGBG`,
then dark). Set `FAVETTO_THEME=dark` or `FAVETTO_THEME=light` to override it.
There is no theme file or picker.

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
spawn = "child_task"                    # optional — fan out from a handoff file
spawn_file = ".favetto/{{ task.id }}/manifest.json"  # JSON array → one child per item
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
  its `finished` event (mutually exclusive with `schedule`). The finished task's
  result is attached as `input._prev`, reachable in the prompt as
  `{{ prev.output }}` / `{{ prev.session_id }}`.
- `spawn` + `spawn_file` chain tasks: when this task succeeds, the JSON file at
  `spawn_file` is read and its array is fanned out — one `spawn`-named task is
  enqueued per element, with that element as its `input`. An empty array spawns
  nothing, so a task can decline to continue.

### Input variables (`[[vars]]`)

A task can declare manual input variables in the TOML header. Starting it from
the TUI opens a floating form that prompts for each one; the collected values
become the task's `input` and render through `{{ input.<name> }}` exactly like
the values produced by `spawn`/`needs`:

```toml
agent = "opencode"

[[vars]]
name = "issue_description"            # becomes input.issue_description
prompt = "Describe the issue to file" # label shown in the form
multiline = true                      # Enter inserts a newline; Ctrl+Enter submits
required = true

[[vars]]
name = "repo"
prompt = "Target repository"
default = "oknozor/favetto"
required = false

[[vars]]
name = "count"
prompt = "How many items"
type = "int"                          # string (default) | int | bool

[[vars]]
name = "flavor"
prompt = "Flavor"
choices = ["vanilla", "mint"]         # rendered as a selectable list
---
```

- `name` (required) — the `input` key. Must match `[a-zA-Z0-9_]+`, be unique
  within the file, and must not be `_prev` (reserved for the `needs`
  predecessor). An invalid declaration makes the whole file fail to parse, so
  the live catalog keeps the previous definition.
- `prompt` (required) — the label/question shown in the form.
- `default` (optional) — the value pre-filled in the form.
- `required` (optional, default `false`) — submission is blocked while the field
  is empty, and unattended runs (hook / `needs` / `spawn` / cron / raw RPC) fail
  with `task '<name>' requires input variable '<var>'` when it is absent.
- `multiline` (optional, default `false`) — `Enter` inserts a newline and the
  form is submitted with `Ctrl+Enter` (or `Tab` from the last field).
- `type` (optional, default `"string"`; one of `string`, `int`, `bool`) — the
  value is parsed before being stored, so `{{ input.count }}` renders a JSON
  number/bool rather than a quoted string. A non-numeric `int` blocks submission.
- `choices` (optional) — a fixed list rendered as a selectable list instead of a
  free-text field.

Variables share the `input` namespace: omit an optional field and its key is
left out, so `{{ input.x }}` renders empty (the existing behavior). The form
never prompts for spawned children or other unattended starts — pass the values
in the element/payload instead.

### Prompt templates

Task prompt bodies and `spawn_file` paths are rendered before use. `{{ dotted.path }}`
placeholders resolve against a JSON context: `{{ task.id }}`, `{{ task.name }}`,
`{{ input.* }}` (the task's input JSON), and `{{ prev.* }}` (the `needs`
predecessor; `input._prev` is reserved). Strings render raw, objects/arrays as
compact JSON, and missing paths as empty. Double braces leave single braces in
prompt code blocks alone.

This makes a triage → plan → implement pipeline declarative: the triage task
writes a `manifest.json` array of issues and `spawn`s the planning task once per
issue; each planning task writes a `handoff.json` and `spawn`s the implementation
task. See `tasks/triage_cocogitto_issues.md`, `tasks/plan_cocogitto_issue.md`, and
`tasks/implement_cocogitto_issue.md`. Favetto ships the same pipeline for its own
issues in `tasks/triage_favetto_issues.md`, `tasks/plan_favetto_issue.md`, and
`tasks/implement_favetto_issue.md`.

The daemon loads the catalog from `--tasks-dir` (default `tasks/`). It also
watches that directory (via `notify`, with a 200 ms debounce): adding, editing,
or removing a `.md` file reloads the catalog on the fly. A file that fails to
parse keeps its previous definition (so a half-written edit never drops a task),
and the recurring tasks derived from `schedule` headers are re-synced — a changed
cron is re-registered and a removed header deletes the `catalog:<name>` schedule,
while manually created schedules are left alone. Every reload pushes
`catalog.updated` so attached TUIs re-fetch the list and refresh the preview.
Runs already in flight keep the definition they were dispatched with; a queued
task picks up the freshly loaded definition when it starts. From the TUI, the
**Catalog** tab lists the catalog and Enter starts a task; a task that declares
`[[vars]]` opens a floating form first (Enter advances, `Ctrl+Enter` submits,
`Esc` cancels and starts nothing), while a var-free task starts immediately. The
Ctrl+P menu's "Add task" writes a new `.md` file (it does not run it). Task
lifecycle emits `task_idle` / `task_started` / `task_finished` events.

The Catalog tab shows a **preview side panel** for the highlighted task: its raw
`.md` source with the TOML front-matter highlighted as TOML and the prompt rendered
as Markdown (headings, lists, blockquotes, fenced code, inline code/bold/italic/
links). Moving the selection with ↑/↓ loads the preview for the new task.

The Events tab is a selectable, scrolling list (↑/↓, PageUp/PageDown) with a
**payload side panel** that pretty-prints the selected event's JSON with syntax
highlighting (keys, strings, numbers, booleans/null).

## Milestones

| Milestone | Scope | Status |
|-----------|-------|--------|
| **M1** | Skeleton + remote TUI: daemon, SQLite, event bus, MessagePack wire protocol over Unix socket + WebSocket, token auth, ratatui client | ✅ Done |
| **M2** | External agent sessions: PTY spawn, server-side `vt100` emulation, remote attach/reattach, terminal query replies | ✅ Done |
| **M3** | Scheduler + notifications: cron, task queue → agent sessions, notification channels, Scheduler + Notifications TUI tabs | ✅ Done |
| **M4** | Hardening: metrics, pairing, webhook receivers + config-driven rules, parallel git-worktree execution | ✅ Done |

## Webhooks & hooks

GitHub signs with `X-Hub-Signature-256`; Linear signs with `Linear-Signature`.
Both are HMAC-SHA256 over the raw body, verified in constant time.

### GitHub triggers (config-driven)

Declare trigger rules in `config.toml`; GitHub POSTs a signed event to
`/webhooks/github` and every matching rule enqueues its catalog task with a
**truncated summary** of the event as the task's `input`:

```toml
[webhook.github]
enabled = true
secret_env = "GITHUB_WEBHOOK_SECRET"   # preferred; or `secret = "…"` (discouraged)

[[webhook.github.rules]]
name = "triage-opened-issues"
event = "issues"                       # X-GitHub-Event
action = "opened"                      # optional; unset matches any action
task = "triage_favetto_issues"         # catalog task to enqueue
filter = { repo = "oknozor/*" }        # optional

[[webhook.github.rules]]
name = "plan-bug-prs"
event = "pull_request"
action = "opened"
task = "plan_favetto_issue"
filter = { labels_contains = ["bug"], base_ref = "main", author = "oknozor" }
```

- **Secret precedence**: literal `secret`, else the env var named by
  `secret_env` (default `GITHUB_WEBHOOK_SECRET`). Linear always reads
  `LINEAR_WEBHOOK_SECRET`. A configured GitHub secret without `enabled = true`
  leaves the endpoint disabled (404).
- **Supported events**: `issues`, `issue_comment`, `pull_request`,
  `pull_request_review`, `push`, `workflow_run`, `check_suite`, `check_run`
  (the issue/PR lifecycle actions map to the `issue_*` / `pr_*` event kinds;
  `workflow_run/completed` reuses `action_run_completed`). Unrecognised events
  are ack'd with 200 and ignored; a rule naming an unsupported event, an unknown
  catalog task, or an invalid glob is a **startup error**.
- **Summary fields** (never the raw body; title capped at 256 bytes, body at
  1024, truncated on UTF-8 boundaries): `event`, `action`, `repo`, `repo_owner`,
  `repo_name`, `number`, `title`, `body`, `author`, `html_url`, `labels`,
  `base_ref`, `head_ref`, `ref`, `commit_count`, `installation_id`,
  `organization`. Prompts reach them as `{{ input.repo }}`, `{{ input.number }}`,
  `{{ input.title }}`, `{{ input.author }}`, `{{ input.labels }}`, ….
- **Filters**: `repo` / `author` / `base_ref` / `head_ref` are globs (`*` also
  crosses `/`, so `oknozor/*` matches `oknozor/favetto`); `labels_contains` is
  any-of exact. Unset fields match anything; all set fields must match (AND).
- **Idempotency**: deliveries are deduplicated on `X-GitHub-Delivery`, and each
  enqueued task gets a delivery-scoped dedupe key, so a redelivery never
  double-runs a task.
- **Restart required**: rules and the secret are read at daemon startup; editing
  `config.toml` needs a restart (there is no config hot-reload).

Local testing:

```bash
# terminal 1
GITHUB_WEBHOOK_SECRET=topsecret ./target/debug/favetto daemon

# terminal 2 — forward real events (needs the GitHub CLI)
gh webhook forward --events=issues --url=http://127.0.0.1:7878/webhooks/github

# or craft a signed delivery with curl + openssl
BODY='{"action":"opened","issue":{"number":1,"title":"hi"},"repository":{"full_name":"oknozor/favetto"},"sender":{"login":"oknozor"}}'
SIG=$(printf '%s' "$BODY" | openssl dgst -sha256 -hmac topsecret | awk '{print $2}')
curl -X POST http://127.0.0.1:7878/webhooks/github \
  -H "X-GitHub-Event: issues" -H "X-GitHub-Delivery: local-1" \
  -H "X-Hub-Signature-256: sha256=$SIG" -d "$BODY"
```

### Notification hooks

Notification hooks are **in-memory**: the TUI adds them via the Ctrl+P "Create a
notification (hook)" wizard (the `hooks.upsert` RPC). They run against every
persisted event and `notify` sends through a channel (e.g. `channel = "webhook"`,
`config = { url = "http://…" }`). They are not persisted and are lost on restart;
task triggers are configured declaratively with `[webhook.github]` above.

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

GitHub webhook secrets resolve from `[webhook.github]` (`secret`, else the env
var named by `secret_env`, else `GITHUB_WEBHOOK_SECRET`); Linear reads
`LINEAR_WEBHOOK_SECRET`. See [Webhooks & hooks](#webhooks--hooks).

### Sound notifications

The TUI can play a short cue when tasks finish. Sounds are produced **on the
client** (the machine running `favetto tui`), work with a remote daemon, and
never block the render/key loop — cues are queued and played by a background
worker. They work out of the box: by default, `task_finished` plays an ascending
chime on success and a descending tone on failure, synthesised to a temporary
WAV at first use (no bundled audio files). If no player is found on `PATH`, the
terminal BEL (`\x07`) is used.

Trigger mapping:

| Event | Cue | Default |
|-------|-----|---------|
| `task_finished` with `success: true` | success chime | on |
| `task_finished` with `success: false` | failure tone | on |
| `task_started` | blip | off |
| `agent.exit` | attention ping | off |

Only `task_finished` is sounded — `task_completed`/`task_failed` never add a
duplicate. Cues that arrive within `min_interval_ms` are coalesced (failure wins
the merge). Press **M** to mute/unmute for the session; the status bar shows a
`sound`/`muted`/`sound off` badge.

```toml
# ~/.config/favetto/config.toml
[tui.sound]
enabled = true                 # master switch (default true)
player = "auto"                # "auto" | "bell" | "command"
# command = "paplay {file}"    # player = "command"; {file} is the sound path
# sound_dir = "~/.config/favetto/sounds"
# min_interval_ms = 400        # coalesce bursts
# only_when_unfocused = false  # play only while the terminal is unfocused

[tui.sound.events]
task_finished = "success"      # built-in name, .wav path, "bell", or "none"
task_failed   = "failure"
task_started  = "none"
attention     = "none"
```

Precedence is **CLI flag → env var → `[tui.sound]` → built-in default**:

- `--sound` / `--no-sound` force the master switch;
  `--sound-command '<cmd> {file}'` sets a custom player.
- `FAVETTO_SOUND` (`1/true/on/yes` or `0/false/off/no/none`) overrides
  `enabled`; `FAVETTO_SOUND_DIR` overrides `sound_dir`;
  `FAVETTO_SOUND_COMMAND` overrides the player.
- `player = "auto"` detects, in order: Linux `paplay`, `pw-play`, `aplay`,
  `ffplay`, `canberra-gtk-play`; macOS `afplay`; Windows PowerShell; else BEL.

Run `favetto tui --test-sound` to play every configured cue once, print the
resolved player and per-cue mapping, and exit.

## Embedded agents

favetto does not reimplement a coding agent: it runs a configured agent CLI
(opencode, Claude Code, pi, Mistral Vibe, …) in a PTY on the daemon and embeds
its live terminal in the TUI's **Agent** tab.

Each CLI is behind a generic `Agent` implementation (`OpenCodeAgent`,
`ClaudeAgent`, `PiAgent`, `VibeAgent`) selected by a registry. A `[agents.*]`
entry whose name is one of `opencode`/`claude`/`pi`/`vibe` uses that built-in and
only needs the fields it wants to override — the shipped defaults are exactly the
example below. Set `type = "opencode"` (etc.) to bind a built-in to a custom name,
or `type = "configurable"` to force the template-only agent for any name. An
entry with no `type` and a non-built-in name is `configurable`. Unknown `type`
values fail at daemon startup.

The built-ins advertise capability flags (interactive, headless, resume, model
selection, providers, …) that the daemon and the one-shot wizard use to adapt;
`agents.list` reports them on the wire.

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
clicks on its own regions — the tab bar (switches tabs) and the row lists in the
Tasks, Catalog, Events, Scheduler and Notifications panels. A left click selects
a row; a second click on the already-selected row runs its primary action
(Tasks open their agent, Catalog tasks start). Clicks on headers, borders or empty
space below the rows are ignored, and all clicks are suppressed while a popup is
open. The wheel scrolls a list's selection, except over the Catalog preview pane
where it scrolls the preview. Clicks in the terminal area are forwarded to the
agent as mouse reports whenever the agent has enabled mouse reporting. This
requires the daemon to stream `state_formatted` frames, which carry the agent's
input modes alongside the screen contents.

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

### Help

Press **?** to open a floating overlay listing every keybinding, grouped by
context (global, lists, Tasks/Catalog, Agent, popups, mouse). Press **?** or
**Esc** to close it, and **↑**/**↓** or **PageUp**/**PageDown** to scroll. `?` is
forwarded to the embedded agent while the Agent panel has keyboard capture, and
is typed literally into form/wizard fields when one is open.

### One-shot tasks

The wizard lists agents from the config, then providers and models from the
`favetto-providers` catalog: providers are the ones authenticated with opencode
(read from `~/.local/share/opencode/auth.json`) and their models come from the
models.dev catalog (`https://models.dev/api.json`, overridable with
`OPENCODE_MODELS_URL`). Provider discovery is **agent-scoped**: `providers.list`
takes an `agent` and returns an empty list for an agent without a provider source.
The daemon caches each agent's catalog for its lifetime. The wizard is
capability-driven: it sends the selected agent's name and skips the provider and
model steps for an agent whose capabilities do not include them (for example
`pi`). The directory step is prefilled with the daemon's working directory. On
completion favetto creates a task row (input carries the inline definition, no
`.md` file), emits `task_idle`/`task_started`, and opens an interactive session
using the agent's `interactive_model_args` (so a model can be applied even when
the agent's main TUI has no model flag — for opencode, `mini --model
provider/model`). The task is marked completed/failed when that session exits.

## Remote API

The daemon exposes one wire protocol over two transports:

- **Unix socket** `/tmp/favetto.sock` (local, trusted).
- **WebSocket** `ws://127.0.0.1:7878/rpc` (bearer token required).

Methods: `system.ping`, `tasks.list`, `tasks.start`, `tasks.cancel`,
`events.tail`, `events.subscribe`, `agents.list`, `agents.start`, `agents.input`,
`agents.resize`, `agents.attach`, `agents.close`, `providers.list`,
`schedules.list`, `schedules.upsert`, `schedules.delete`, `notifications.list`,
`notifications.test`, `hooks.upsert`. Server pushes:
`event`, `task.updated`, `catalog.updated`, `log.line`, `agent.output`,
`agent.exit`.

The daemon also serves HTTP endpoints: `GET /metrics` (Prometheus),
`POST /pair/generate` and `POST /pair/exchange` (pairing), plus the webhook
receivers.
