# Embedded agents

favetto does not reimplement a coding agent: it runs a configured agent CLI
(opencode, Claude Code, pi, Mistral Vibe, …) in a PTY on the daemon and embeds
its live terminal in the TUI's **Agent** tab.

```bash
# see what is registered, and whether each binary is installed
favetto daemon   # then, in the TUI, the Tasks tab lists agents and sessions
```

![favetto TUI — Agent tab](/screenshots/tui-agent.png)

*The Agent tab: the embedded agent session, seeded with the task prompt.*

## Configure an agent

The four built-ins are always registered, so an empty config already exposes
them. A `[agents.*]` entry is an **overlay**: it customises a built-in's
invocation, binds a built-in to a custom name, or adds a custom CLI. Every field
you set is merged over the built-in default; omitted fields keep it.

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

The fields are documented exhaustively in the
[configuration reference](../reference/config#agentconfig). In short:

- `args` is the interactive invocation.
- `headless_args` runs catalog tasks unattended; `run_args` is used instead when
  the task sets a `model`, with `{prompt}`, `{provider}`, and `{model}`
  substituted so the model flag can sit where the CLI expects it.
- `prompt_args` seeds an interactive session with the task prompt (some agents
  only pre-fill their input, so `submit_prompt = true` sends Enter once the UI
  has settled). With no `prompt_args`, the prompt is written to stdin.
- `resume_args` reopens a previously run session (`{session_id}`), and
  `session_id_json_key` captures the id from the run's line-delimited JSON.

A `[agents.*]` entry whose name is one of `opencode`/`claude`/`pi`/`vibe` uses
that built-in. Set `type = "opencode"` (etc.) to bind a built-in to a custom
name, or `type = "configurable"` to force the template-only agent. An entry with
no `type` and a non-built-in name is `configurable`; an unknown `type` fails at
daemon startup.

## Model selection

Model selection lives in the invocation templates, because many CLIs (for
example opencode) only accept `--model` on a subcommand such as `run`. When a
task sets `model`, favetto uses `run_args`; otherwise it uses `headless_args` and
the agent's own default model applies. `{provider}` and `{model}` are the task's
separate provider/model values; a `provider` without a `model` is ignored.

```md
agent = "opencode"
provider = "jev"
model = "1.13"
---
Fix the failing test.
```

## Availability

The daemon probes its `PATH` for each agent's executable once at startup and
reports an `available` flag on `agents.list`. An agent whose binary is not
installed is still listed but marked unavailable, and launching it fails with a
clear error. The one-shot wizard dims unavailable agents with `(not installed)`
and skips over them. Availability is resolved once, so a `PATH` change needs a
daemon restart.

## The Agent panel

From the **Tasks** tab, highlight a task and press **Enter** to open its agent
session, seeded with the task prompt and running in the task's workspace. Opening
a task first reattaches to its running session; if the run already finished and a
session id was captured, it reopens that session with `resume_args`. Press
**Ctrl+N** to force a new session.

When a task's row shows **awaiting input** (see [Lifecycle](#lifecycle)), pressing
**Enter** attaches to the *live blocked PTY* rather than launching a fresh
session, so anything you type — a permission key, a passphrase — reaches the
prompt the agent is waiting on.

The daemon keeps a `vt100` emulator per session and streams self-contained
full-screen frames; the panel parses and renders them, keys are forwarded to the
agent, and the PTY is resized to fit. Common terminal queries (cursor position,
device attributes, colours, mode reports) are answered by the daemon on the PTY.

- **Keyboard focus.** The panel captures the keyboard by default, so the agent
  receives every keystroke. Press **Ctrl+Y** to toggle between the agent and
  favetto; the status bar shows `focus: agent` / `focus: favetto`. With favetto
  focused, **Ctrl+Q** leaves the panel (the session keeps running) and **Ctrl+N**
  starts a new session.
- **Mouse.** Clicks on favetto's own regions (tab bar, row lists) are handled by
  favetto: a click selects a row and a second click runs its primary action.
  Clicks in the terminal area are forwarded to the agent as mouse reports when it
  has enabled them.

## Lifecycle

Agents launch through a small supervisor (`favetto __agent-exec`) that owns a
process group and dies with the daemon — on both `SIGTERM` and `SIGKILL` — taking
the agent and its children with it. Live PTYs therefore do **not** survive a
daemon restart, but an agent's own session (captured via `session_id_json_key`)
does, so tasks can be resumed after a restart.

Catalog tasks run through their `agent` (or `[agent].default`) in headless mode
using `headless_args` (or `run_args` when a model is set); a task with no agent
and no default fails to start. An external agent **without** the relevant args is
rejected rather than launched interactively (an interactive TUI would never exit
and would hang the task).

While a run is in flight the daemon watches its terminal screen. If the agent
blocks on something only a human can answer — a tool permission dialog, a
confirmation, a multiple-choice prompt, or a git pinentry/askpass — the task is
marked **awaiting input** (a non-terminal status), a `task_awaiting_input` event
is emitted so notification hooks fire, and the TUI plays its attention cue. Once
you answer in the Agent panel the task returns to **running** and continues. The
built-in `opencode` and `claude` detectors use each CLI's own dialog wording;
custom agents fall back to a conservative heuristic that requires the PTY to be
quiet for `[executor].awaiting_input_quiet_ms` (default 8 s) and the last visible
lines to look like a prompt. Set `[executor].detect_awaiting_input = false` to
turn detection off.
