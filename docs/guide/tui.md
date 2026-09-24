# TUI

The TUI is a client of the daemon. This page covers its tabs, keybindings, and
client-side behaviours (theme, sound, the Ctrl+P menu). For the Agent panel's
focus and mouse handling see [Embedded agents](./agents).

![favetto TUI — Catalog tab](/screenshots/tui-catalog.png)

*The Catalog tab: folder tree + preview pane.*

![favetto TUI — Tasks tab](/screenshots/tui-tasks.png)

*The Tasks tab: recent runs with status, age, and session title.*

An opencode run writes its session title a few seconds after its first turn. As
soon as the agent reports its session id **during** the run, the daemon resolves
the title and pushes it to connected clients, so the `SESSION` cell fills in
live without restarting the TUI. If the title is not available in time, the
completion-time bounded retry and the background backfill still fill it a few
seconds later. A failed run keeps the session id and title it captured. One-shot
tasks resolve their title the same way when the agent reports a session id.
Agents that never expose titles (`claude`, `pi`, `vibe`, custom) show a blank
cell and skip the retry.

A task whose agent blocks on a permission prompt, confirmation, choice, or git
pinentry is shown as **awaiting input** (glyph `!`, warning colour) instead of
silently staying `running`, and the status bar counts them. Press **Enter** on
that row to open the Agent panel and answer the prompt; the task returns to
`running` once the agent continues.

## Keybindings

Press **?** at any time to open the same reference as a floating overlay. `?` or
`Esc` closes it; `↑`/`↓` or `PageUp`/`PageDown` scroll it.

| Context | Key | Action |
|---------|-----|--------|
| Global | `Tab` / `→` | next tab |
| Global | `Shift+Tab` / `←` | previous tab |
| Global | `Ctrl+P` | open the action menu |
| Global | `Ctrl+O` | pick a live/retained agent session |
| Global | `?` | open/close help |
| Global | `w` | open/close the workflow graph |
| Global | `M` | mute/unmute sound |
| Global | `q` / `Esc` | quit |
| Lists | `↑` / `↓` | move selection |
| Tasks | `Enter` | open the selected task's agent session |
| Tasks | `c` | cancel the selected non-terminal task (confirm) |
| Tasks | `r` | retry the selected terminal task (confirm) |
| Tasks | `C` | cancel every task in the selected task's workflow root (confirm) |
| Tasks | `i` | inspect the selected task's runtime workflow |
| Catalog | `Enter` | start the task; on a folder, fold/unfold |
| Catalog | `e` | edit the selected task in `$VISUAL`/`$EDITOR` |
| Catalog | `Space` | fold/unfold the selected folder |
| Catalog | `p` | show/hide the preview pane |
| Catalog | `PageUp` / `PageDown` / wheel | scroll the preview pane |
| Events | `PageUp` / `PageDown` | move the selection by a page |
| Usage | `1` | day buckets (last 24 hours) |
| Usage | `2` | week buckets (last 7 days) |
| Usage | `3` | month buckets (last 30 days) |
| Usage | `4` | year buckets (last 12 months) |
| Agent | `Ctrl+Y` | toggle focus: agent ↔ favetto |
| Agent | *(agent focus)* | every key is forwarded to the agent |
| Agent | *(favetto focus)* `Ctrl+O` | switch to another session |
| Agent | *(favetto focus)* `Ctrl+Q` | leave the panel |
| Agent | *(favetto focus)* `Ctrl+N` | start a new session |
| Agent | *(favetto focus)* `Esc` | back to Tasks |
| Popups | `↑` / `↓` | select |
| Popups | `Enter` | confirm / next field |
| Popups | `Esc` | cancel / close |
| Text fields | `←` / `→` | move the caret |
| Text fields | `Alt+←` / `Alt+→` (or `Ctrl+←`/`Ctrl+→`) | move by word |
| Text fields | `Home` / `End` (or `Ctrl+A` / `Ctrl+E`) | start / end of line |
| Text fields | `Backspace` / `Delete` | delete before / at the caret |
| Text fields | `Alt+Backspace` / `Ctrl+W` | delete the word before the caret |
| Text fields | `Ctrl+U` / `Ctrl+K` | delete to the start / end of the line |
| Task input | `↑` / `↓` | move the caret between lines in multiline values |
| Task input | `Shift+Enter` / `Alt+Enter` | insert a newline in multiline values |
| Task input | `Ctrl+Enter` | submit the form |
| Workflow (`w`) | `↑` / `↓` / `PageUp` / `PageDown` | scroll the graph source |
| Workflow (`w`) | `Esc` / `w` | close |
| Runtime workflow (`i`) | `↑` / `↓` | select an instance |
| Runtime workflow (`i`) | `c` | cancel the root |
| Runtime workflow (`i`) | `r` | retry the selected instance |
| Runtime workflow (`i`) | `Esc` / `i` | close |
| Mouse | click the tab bar | switch tabs |
| Mouse | click a catalog folder | fold/unfold it |
| Mouse | click a catalog/task row | select; click again starts it |

`?`, `w`, `i`, `p`, `e`, `c`, `r`, and `C` are forwarded to the embedded agent
while the Agent panel has keyboard capture, and are typed literally into
form/wizard fields when one is open. `p` only toggles the preview and `e` only
edits on the Catalog tab; `i` only inspects on the Tasks tab, and `c`, `r`, and
`C` only act on the Tasks tab.

## Cancel and retry

On the **Tasks** tab you can act on the selected row without reaching for the API:

- **c** cancels a non-terminal task (pending, running, or awaiting input).
- **r** retries a terminal task (succeeded, failed, or cancelled), recording a new
  attempt and keeping the prior run history.
- **C** cancels *every* non-terminal task in the selected task's workflow root, so
  a whole `spawn`/`needs` pipeline can be abandoned. It is offered only for a task
  that belongs to a workflow root (a spawned child, i.e. `root_id` is set).

Each key opens a confirmation popup first: `Enter`/`y` confirms, `Esc`/`n`
dismisses. Nothing is applied optimistically — the daemon pushes `task.updated`
for every changed task, so the row (or rows, for `C`) updates live. A rejected
command (for example retrying a task whose run is still live) is reported in the
status bar.

## Agent session picker

Press **Ctrl+O** to list every live and retained agent session on the daemon
(`agents.list`) and hop between concurrent runs — for example an `opencode` task
and a `pi` task running at the same time. `↑`/`↓` moves the selection, **Enter**
attaches the Agent panel to the chosen session (`agents.attach`), and `Esc` (or
`Ctrl+O` again) closes the picker. Each row shows the session's agent, whether it
is running or finished, and whether it is a headless run. `Ctrl+O` is forwarded to
the agent while the panel has keyboard capture, like the other favetto keys.

If accepting the picker or opening a task fails, the error is shown in the status
bar (the same place the connection state and counts live) rather than only in the
internal log ring.

## Ctrl+P menu

Press **Ctrl+P** for guided, step-by-step forms:

- **New one-shot task** — a wizard (agent → provider → model → directory) that
  starts an inline, interactive task and opens it in the Agent panel for you to
  prompt. It emits the normal task events but is **not** written to the catalog.
- **Add task to catalog** — writes a task `.md` file (does not run it).
- **Create a schedule event** — adds a cron schedule.
- **Create a notification (hook)** — adds a hook that reacts to an event kind and
  sends a notification through a channel.

### One-shot wizard

The wizard lists agents from the config, then providers and models from the
`favetto-providers` catalog: providers are the ones authenticated with opencode
(read from `~/.local/share/opencode/auth.json`) and their models come from the
models.dev catalog (`https://models.dev/api.json`, overridable with
`OPENCODE_MODELS_URL`). Provider discovery is **agent-scoped**: `providers.list`
takes an `agent` and returns an empty list for an agent without a provider
source, and the daemon caches each agent's catalog for its lifetime.

The wizard is capability-driven: it sends the selected agent's name and skips the
provider and model steps for an agent without a provider catalog (for example
`pi`, `claude`, or `vibe`). Catalog tasks for those agents can still select a
model through the task's `provider`/`model` header keys, which render
`run_args`. The directory step is prefilled with the daemon's working
directory. On completion favetto creates a task row (the input carries the inline
definition — no `.md` file), emits `task_idle`/`task_started`, and opens an
interactive session using the agent's `interactive_model_args`. The task is
marked completed/failed when that session exits.

A catalog task started with **Enter** on the Catalog tab runs the same way: the
daemon launches the agent's real TUI (seeded with the rendered prompt) and the
Agent panel attaches to it live and writable. The task is marked
completed/failed as soon as the agent finishes the seeded turn, so a `spawn` or
`needs` pipeline continues without you having to quit the CLI; the TUI is then
left open so you can still inspect it or keep working by hand. Agents whose
interactive mode exits on its own are completed/failed when the session exits
instead. Only programmatic runs (schedules, hooks/webhooks, `needs`, `spawn`)
stay headless and finish on their own.

Opening the Agent panel on a running task never shows the headless machine output
(raw JSON). For **opencode** the daemon attaches a genuine interactive session to
the run's session on its managed server while the headless process keeps running
in the background; for agents that cannot attach concurrently it renders the
structured state view (activity, usage, session id) instead. A headless run that
is blocked on a user answer is the exception: the panel shows its screen so you
can type the answer.

## Workflow overlay

Press **w** to render the catalog's `needs`/`spawn` edges as box-drawing art (the
same graph is persisted as Graphviz DOT to `<data_dir>/workflow.dot`).

![favetto workflow graph](/screenshots/workflow-graph.png)

*The workflow overlay: `needs` and `spawn` edges between tasks.*

Nodes are labelled with their task name, scheduled tasks show `(scheduled)`, and
references to names outside the catalog show `(external)`. Scroll with
`↑`/`↓`/`←`/`→` (and `PageUp`/`PageDown`). If the structured graph is
unavailable (an older daemon) or cannot be rendered, the overlay falls back to
the raw DOT with a short explanation. It refreshes while open when the catalog
changes.

## Runtime workflow inspector

Press **i** on the Tasks tab to inspect the **selected task's root** live through
`workflow.inspect` — the catalog graph above shows the *declared* tasks, while
this overlay shows the durable runtime instances of one root (whether created by
`needs`/`spawn` or by `workflow.create`/`workflow.spawn`). The title shows the
root task and its overall state (`running`, `succeeded`, `failed`, `cancelled`),
followed by a line of `ready`/`running`/`failed`/`blocked` bucket counts and a
per-instance table with each instance's **status**, **task name**, **attempt**,
and **summary** (the bounded failure/cancellation reason).

`↑`/`↓` moves the instance selection, **c** cancels every non-terminal instance
in the root (`workflow.cancel`), and **r** retries the selected instance
(`workflow.retry`). `Esc` or `i` closes the overlay. The view is summary-only —
it never fetches `output` blobs — and refreshes as the daemon pushes
`task.updated`/`event` frames, so instances appear and change state while it is
open. A directly started task is its own root; a spawned child inspects the root
it inherited.

## Events panel

The **Events** tab is a selectable, scrolling list (`↑`/`↓`, `PageUp`/`PageDown`)
with a **payload side panel** that pretty-prints the selected event's JSON with
syntax highlighting.

![favetto TUI — Events tab](/screenshots/tui-events.png)

*The Events tab with the payload panel.*

## Usage panel

The **Usage** tab shows where tokens and money went. Each finished attempt
persists its `AgentUsage` (input/output/reasoning/cache tokens and cost) in flat
columns on its run row, independent of the `tasks.output` blob, so the data
survives the retention pass that clears old output. The panel calls the typed
`usage.stats` RPC, which aggregates finished runs into a fixed number of time
buckets plus window totals (runs, tokens, cost).

Press **1**–**4** to pick the period; the panel re-fetches and the charts and
labels update:

| Key | Period | Buckets | Window |
|-----|--------|---------|--------|
| `1` | Day | hourly | last 24 hours |
| `2` | Week | daily | last 7 days |
| `3` | Month | daily | last 30 days |
| `4` | Year | monthly | last 12 calendar months |

The header summarises run count, total tokens (with the input/output split) and
total cost for the window. Below it, a **Tokens** `BarChart` and a separate
**Cost (USD)** `BarChart` render one bar per bucket (different units read better
as two charts); empty buckets are still drawn so the axis stays stable. Costs
are shown to four decimal places. Attaching to the tab, changing the period, or
a `task.updated` push all refresh the view.

## Theme

The TUI selects a dark or light theme from the terminal background (an OSC 11
query, then `COLORFGBG`, then dark). Set `FAVETTO_THEME=dark` or
`FAVETTO_THEME=light` to override it. There is no theme file or picker.

## Sound notifications

The TUI can play a short cue when tasks finish. Sounds are produced **on the
client** (the machine running `favetto tui`), work with a remote daemon, and
never block the render/key loop — cues are queued and played by a background
worker. They work out of the box: by default, `task_finished` plays an ascending
chime on success and a descending tone on failure, synthesised to a temporary WAV
at first use (no bundled audio files). If no player is found on `PATH`, the
terminal BEL (`\x07`) is used.

| Event | Cue | Default |
|-------|-----|---------|
| `task_finished` with `success: true` | success chime | on |
| `task_finished` with `success: false` | failure tone | on |
| `task_started` | blip | off |
| `task_awaiting_input` | attention ping | on |
| `agent.exit` | attention ping | off |

Only `task_finished` is sounded — `task_completed`/`task_failed` never add a
duplicate. Cues within `min_interval_ms` are coalesced (the highest-priority cue
wins; `task_awaiting_input` outranks a failure). Press **M** to mute/unmute for
the session; the status bar shows a `sound`/`muted`/`sound off` badge.

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
awaiting_input = "attention"   # agent blocked on a prompt
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

![favetto help overlay](/screenshots/tui-help-overlay.png)

*The `?` help overlay, grouped by context.*
