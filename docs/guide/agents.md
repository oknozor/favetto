# Embedded agents

favetto does not reimplement a coding agent: it runs a configured agent CLI
(opencode, Claude Code, pi, Mistral Vibe, …) in a PTY on the daemon and embeds
its live terminal in the TUI's **Agent** tab.

Each CLI is behind a generic `Agent` implementation (`OpenCodeAgent`,
`ClaudeAgent`, `PiAgent`, `VibeAgent`) selected by a registry. The four built-ins
are always registered, so even an empty config exposes them; a `[agents.*]` entry
overrides one, binds a built-in to a custom name, or adds a custom CLI. A
`[agents.*]` entry whose name is one of `opencode`/`claude`/`pi`/`vibe` uses that
built-in and only needs the fields it wants to override — the shipped defaults are
exactly the example below. Set `type = "opencode"` (etc.) to bind a built-in to a
custom name, or `type = "configurable"` to force the template-only agent for any
name. An entry with no `type` and a non-built-in name is `configurable`. Unknown
`type` values fail at daemon startup.

The built-ins advertise capability flags (interactive, headless, resume, model
selection, providers, …) that the daemon and the one-shot wizard use to adapt;
`agents.list` reports them on the wire. The daemon also probes its `PATH` for each
agent's executable once at startup and reports an `available` flag: an agent whose
binary is not installed is still listed but marked unavailable, and launching it
fails with a clear error. The one-shot wizard dims unavailable agents by appending
`(not installed)` and skips over them, so they cannot be selected. Availability is
resolved once, so a `PATH` change needs a daemon restart.

The example below shows overriding the opencode built-in; every field you set is
merged over the built-in default, and omitted fields keep it.

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

## Keyboard focus

When the Agent panel opens it captures the keyboard by default, so the embedded
agent receives every keystroke (including ones favetto would otherwise use). Press
**Ctrl+Y** to toggle focus between the agent and favetto; the active owner is shown
in the bottom status bar (`focus: agent` / `focus: favetto`). With favetto
focused, **Ctrl+Q** leaves the panel and **Ctrl+N** starts a new session. Ctrl+Y is
chosen not to clash with opencode's default keybinds.

## Mouse

Clicks are delivered to both sides without conflict: favetto handles clicks on its
own regions — the tab bar (switches tabs) and the row lists in the Tasks, Catalog,
Events, Scheduler and Notifications panels. A left click selects a row; a second
click on the already-selected row runs its primary action (Tasks open their agent,
Catalog tasks start). Clicks on headers, borders or empty space below the rows are
ignored, and all clicks are suppressed while a popup is open. The wheel scrolls a
list's selection, except over the Catalog preview pane where it scrolls the
preview. Clicks in the terminal area are forwarded to the agent as mouse reports
whenever the agent has enabled mouse reporting. This requires the daemon to stream
`state_formatted` frames, which carry the agent's input modes alongside the screen
contents.

`prompt_args` seeds the prompt (`{prompt}` is substituted); some agents only
pre-fill their input, so set `submit_prompt = true` to send Enter once the UI has
settled. With no `prompt_args`, the prompt is written to the agent's stdin instead.

Agents are launched through a small supervisor (`favetto __agent-exec`) that owns a
process group and dies with the daemon — on both `SIGTERM` and `SIGKILL` — taking
the agent and its children with it. Live PTYs therefore do **not** survive a daemon
restart, but an agent's own session (captured via `session_id_json_key`) does, so
tasks can be resumed after a restart.

Catalog tasks run through their `agent` (or `[agent].default`) in headless mode
using `headless_args` (or `run_args` when a model is set); a task with no agent
(and no default) fails to start. An external agent **without** the relevant args
is rejected rather than launched interactively (an interactive TUI would never
exit and would hang the task).
