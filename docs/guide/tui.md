# TUI

The TUI is a client of the daemon. This page covers its tabs, keybindings, and
client-side behaviours (theme, sound, the Ctrl+P palette and the Ctrl+X leader).
For the Agent panel's focus and mouse handling see [Embedded agents](./agents).

![favetto TUI — Catalog tab](/screenshots/tui-catalog.png)

*The Catalog tab: folder tree + preview pane.*

![favetto TUI — Tasks tab](/screenshots/tui-tasks.png)

*The Tasks tab: recent runs with their repo, kind, status, started time, duration, and session title.*

ACP sessions report an agent-side session id, but session titles are not
surfaced yet, so the `SESSION` cell stays blank. The id is still recorded on the
task row.

When a session asks for tool permission it reports `awaiting_input`; open it with
**Enter** and answer with **Ctrl+R**. (The task row stays `running` until the
turn ends.)

Each Tasks row also carries a **KIND** column derived from the catalog
definition: `action` for a built-in deterministic step, `scheduled` for a
cron-triggered task, and `agent` otherwise — with `action` outranking
`scheduled`, which outranks `agent`. The label is definition-based rather than
per-run, so a task started by hand from a scheduled definition still reads
`scheduled`; an unknown or one-shot row shows `—`. See
[the catalog](./catalog) and
[schedules and dependencies](./schedules-and-dependencies).

Press **Enter** (or double-click) on a row to open the run. For an **agent**
task this opens the Agent panel as described above. For an **action** task there
is no agent session to open, so the client fetches the run with `tasks.get` and
shows a small **Action result** popup with its status and the durable
`envelope.summary` the action produced (for example the `http.request` status and
a short description). The daemon also refuses `agents.start` for an action task,
so no client can accidentally launch an agent against one. `Esc` or `Enter`
closes the popup.

## Keybindings

Press **?** at any time to open the same reference as a floating overlay. `?` or
`Esc` closes it; `↑`/`↓` or `PageUp`/`PageDown` scroll it.

| Context | Key | Action |
|---------|-----|--------|
| Global | `Tab` / `→` | next tab |
| Global | `Shift+Tab` / `←` | previous tab |
| Global | `Ctrl+P` | open the command palette |
| Global | `Ctrl+X` | open the shortcut leader, then a key below |
| Global | `Ctrl+X` `n` | new one-shot task |
| Global | `Ctrl+X` `l` | launch a catalog task |
| Global | `Ctrl+X` `a` | add task to catalog |
| Global | `Ctrl+X` `s` | create schedule |
| Global | `Ctrl+X` `r` | switch repo |
| Global | `Ctrl+X` `R` | add repo |
| Global | `Ctrl+X` `t` | change theme |
| Global | `Ctrl+X` `h` | open docs in the browser |
| Global | `Ctrl+O` | pick a live or past agent session |
| Global | `?` | open/close help |
| Global | `w` | open/close the workflow graph |
| Global | `M` | mute/unmute sound |
| Global | `q` / `Esc` | quit |
| Lists | `↑` / `↓` | move selection |
| Tasks | `Enter` | open the selected task: its agent session, or an action task's result summary |
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
| Agent | *(agent focus)* | type a follow-up prompt; `Enter` sends it |
| Agent | *(favetto focus)* `Ctrl+O` | switch to another session |
| Agent | *(favetto focus)* `Ctrl+Q` | leave the panel |
| Agent | *(favetto focus)* `Ctrl+N` | start a new session |
| Agent | *(favetto focus)* `Esc` | back to Tasks |
| Agent | drag with the left button | select output; copies to the clipboard on release |
| Popups | `↑` / `↓` | select |
| Popups | `Enter` | confirm / next field |
| Popups | `Esc` | cancel / close |
| Command palette | characters | filter commands by full-text search |
| Command palette | `↑` / `↓` | move the selection (headers are skipped) |
| Command palette | `Enter` | run the highlighted command |
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

While the Agent panel has keyboard capture, printable keys compose a prompt and
**Enter** sends it with `agents.prompt`. Favetto's own shortcuts (`Ctrl+P`,
`Ctrl+O`, `Ctrl+Y`, `Ctrl+N`, `Ctrl+Q`) still work in the panel. In form/wizard
fields, keys are typed literally.

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

Press **Ctrl+O** to list every live agent session on the daemon
(`agents.list`) **and** the ACP sessions Favetto created (`agents.history`) so
you can hop between concurrent runs or reopen a past conversation.
`↑`/`↓` moves the selection, **Enter** opens the chosen row, and `Esc` (or
`Ctrl+O` again) closes the picker. Each row shows its title (when the agent
reported one), agent, working directory, lifecycle state, and activity/usage
for live sessions.

- **Active** rows reattach the Agent panel to the in-process session
  (`agents.attach`). Retained sessions whose process has exited still attach to
  their in-process transcript rather than loading.
- **Ended** rows that are loadable restore the provider's conversation with
  `agents.load`, addressed by the **Favetto-local session id** (the provider ACP
  id is never the routing key). The Agent tab shows a restoring status while the
  replay is in flight; the transcript appears only after the load succeeds.
- **Loading**, **failed**, and **cannot load** rows explain themselves in the
  status bar and do nothing. Selecting a past session never starts a fresh
  session, even when the agent does not support ACP `session/load`.

`Ctrl+O` is forwarded to the agent while the panel has keyboard capture, like
the other favetto keys.

If accepting the picker or opening a task fails, the error is shown in the status
bar (the same place the connection state and counts live) rather than only in the
internal log ring.

The picker merges Favetto-created sessions (`agents.history` / `agents.load`) with
provider-owned sessions from the agent's advertised ACP `session/list`.
Provider rows are labelled `provider`; type to filter by title, agent or
directory. Resuming one imports it into a Favetto-local record, and `Ctrl+D`
deletes a provider transcript only when the agent advertises `session/delete`.
Unsupported agents show an inline explanation instead of an action.

## Copy agent output

Drag with the left mouse button over the **Agent** panel to select text; the
selection is highlighted and copied to the clipboard as soon as the button is
released, with no keyboard shortcut. The status bar confirms with
`copied N chars`.

The copy always emits an **OSC 52** escape (so it still works over SSH, `tmux`,
and containers that forward the sequence) and additionally tries the local
clipboard tools `wl-copy`, `xclip`, or `xsel` on Linux, `pbcopy` on macOS, or
`clip` on Windows. When only the escape is available the status bar reads
`copied N chars (OSC 52)`.

A plain click (no drag) selects nothing. The transcript of a finished session
stays selectable.

Press **Ctrl+P** to open the **command palette**: a borderless, vertically
centred panel with a bold **Commands** title, a muted `esc` hint on the right, and
an inline full-text search directly under the title. Type to filter the commands
(the matching is a fuzzy, skim-style full-text match); `↑`/`↓` move the highlight,
**Enter** runs the highlighted command, and `Esc` closes the palette. Commands are
grouped under bold, non-selectable section titles (**Tasks**, **Catalog**,
**Repositories**, **Appearance**, **Help**), with one blank row between sections,
and each command shows its `Ctrl+X` shortcut on the right. Commands dispatch by
identity, so grouping and filtering never change what a command does.

- **New one-shot task** — a wizard (agent → provider → model → directory) that
  starts an inline, interactive task and opens it in the Agent panel for you to
  prompt. It emits the normal task events but is **not** written to the catalog.
- **Launch task** — opens a searchable list of catalog tasks (see
  [Launching a task from the palette](#launching-a-task-from-the-palette)) and
  runs the highlighted one exactly like **Enter** on the Catalog tab.
- **Add task to catalog** — writes a task `.md` file (does not run it).
- **Create a schedule event** — adds a cron schedule.
- **Create a notification (hook)** — adds a hook that reacts to an event kind and
  sends a notification through a channel.
- **Switch repo** / **Add repo** — change the active repo scope or register a new
  repo; see [Repo scoping](#repo-scoping).
- **Change theme** — preview and pick a theme and mode.
- **Open docs** — opens <https://oknozor.github.io/favetto/> in your browser.

### Shortcut leader (Ctrl+X)

Every palette command also has a mnemonic shortcut reached through the **Ctrl+X**
leader, modelled on opencode. Pressing `Ctrl+X` dims the active view and shows a
which-key hint listing the available combinations; the next key runs its command.
`Esc`, an unmapped key, or a two-second timeout cancels the pending leader. The
combinations are:

| Key | Command |
|-----|---------|
| `Ctrl+X` `n` | new one-shot task |
| `Ctrl+X` `l` | launch a catalog task |
| `Ctrl+X` `a` | add task to catalog |
| `Ctrl+X` `s` | create schedule |
| `Ctrl+X` `r` | switch repo |
| `Ctrl+X` `R` | add repo |
| `Ctrl+X` `t` | change theme |
| `Ctrl+X` `h` | open docs in the browser |

The leader works from every tab while favetto owns the keyboard. While the Agent
panel has keyboard capture, the leader still works (the panel only consumes
printable keys and `Enter`), and the `?` overlay documents it under **Global**.

### Launching a task from the palette

**Ctrl+P → Launch task** (or `Ctrl+X` `l`) opens a searchable list of the whole
catalog, rendered in the same palette style: a borderless panel, a bold title, a
muted `esc` hint, and an inline `search:` row. Tasks are grouped under a
**Global** header for unscoped entries and one header per repo for
`repo:<name>/...` entries; typing filters the list with the same fuzzy,
skim-style search as the palette and drops any group left with no match. `↑`/`↓`
move the highlight (headers are skipped), **Enter** launches the highlighted task
through the same path as **Enter** on the Catalog tab — a task with `[[vars]]`
opens its variable form first — and **Esc** closes the picker.

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
instead. Only programmatic runs (schedules, `needs`, `spawn`) stay headless and
finish on their own.

Opening the Agent panel on a running task never starts a second ACP session:
the daemon attaches to that task's existing live session, and the panel backfills
its structured feed from `tasks.events` so the conversation so far is visible.
An ordinary headless run is shown read-only; a run blocked on a user answer stays
writable so the answer can reach it. **N** still creates a deliberately new
session. Once the run has finished, opening the task loads or resumes the
persisted session as usual.

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

## Repo scoping

The **Tasks** and **Events** tables each carry a **REPO** column: the name of
the repo a run or event belongs to, or `global` for a bare task name and for
unscoped integration events. When a specific repo is active (`Ctrl+P → Switch
repo`), both tabs are scoped to that repo's rows only; the selected row is
clamped into view and the mouse hit-testing follows the filtered rows. The
**Catalog** tab is scoped the same way but additionally keeps the global
(unscoped) tasks visible: with a repo active it lists that repo's local tasks
plus the globals, hiding every other repo (a repo with no local tasks shows only
the globals). Selecting `global` restores the full, labelled lists. The active
repo is also shown in the status bar (`repo: <name>` / `repo: global`); see
[Repository-local tasks](./catalog#repository-local-tasks).

## Events panel

The **Events** tab is a selectable, scrolling list (`↑`/`↓`, `PageUp`/`PageDown`)
with a **payload side panel** that pretty-prints the selected event's JSON with
syntax highlighting. Each row shows the event's **REPO** (or `global`); the
repo is derived from the event's task name, or from its `task_id` for the
terminal/cron events that do not carry a name.

![favetto TUI — Events tab](/screenshots/tui-events.png)

*The Events tab with the payload panel.*

## Usage panel

The **Usage** tab shows where tokens and money went. Each finished attempt
persists its `AgentUsage` (input/output/reasoning/cache tokens, cost, and the
model context window when the agent reports one) in flat columns on its run row,
independent of the `tasks.output` blob, so the data survives the retention pass
that clears old output. Token totals come from the ACP `session/prompt`
response; a context-only `usage_update` keeps `context_used`/`context_size` and
never fabricates zero token counters. The panel calls the typed `usage.stats`
RPC, which aggregates finished runs into a fixed number of time buckets plus
window totals (runs, tokens, cost).

Cost is only shown when an agent reported one. A reported `$0.0000` is a genuine
free run; when no run in the window reported a cost the total and its bars show
`—` (unknown), not `$0`.

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

The TUI paints every surface through a semantic design-token theme (background,
text, border, syntax and markdown groups, with raised-surface depth levels).
Two built-in themes ship: **Opencode** (the default) and **Catppuccin**. Each
has a dark and a light variant.

- `[tui].theme` — the theme name. A built-in name, or a user theme loaded from
  `~/.config/favetto/themes/<name>.json` (also searched in the config-local
  `themes/` dir and the repo-local `.favetto/themes/`). Override with
  `FAVETTO_THEME_NAME`.
- `[tui].theme_mode` — `system` (the default; derives dark/light from the
  terminal background via an OSC 11 query, then `COLORFGBG`, then dark), `dark`,
  or `light`. Override with `FAVETTO_THEME=dark|light`.

```toml
# ~/.config/favetto/config.toml
[tui]
theme = "opencode"
theme_mode = "system"
```

Press **Ctrl+P → Change theme** to preview and select a theme or mode for the
session (the choice is not written back to the config). While the picker is
open, ↑/↓ preview the highlighted entry, Enter keeps it, and Esc restores the
previous theme. Set `NO_COLOR` (or run on a dumb terminal) to fall back to a
colourless theme.

A user theme is JSON with optional `dark`/`light` token maps; missing tokens
inherit from the built-in default, and colours may be `#rrggbb`, `transparent`,
or a `$hue.<hue>.<100..900>` reference:

```json
{
  "name": "solarish",
  "dark": {
    "background.base": "#101418",
    "text.action.primary": "#ffcc00"
  }
}
```

Set `[tui].show_header = false` to hide the tab bar and
`[tui].show_footer = false` to hide the status line (both default true).

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
