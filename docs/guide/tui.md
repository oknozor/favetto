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
| Global | `?` | open/close help |
| Global | `w` | open/close the workflow graph |
| Global | `M` | mute/unmute sound |
| Global | `q` / `Esc` | quit |
| Lists | `↑` / `↓` | move selection |
| Lists | `PageUp` / `PageDown` | move or scroll by a page |
| Tasks | `Enter` | open the selected task's agent session |
| Catalog | `Enter` | start the task; on a folder, fold/unfold |
| Catalog | `e` | edit the selected task in `$VISUAL`/`$EDITOR` |
| Catalog | `Space` | fold/unfold the selected folder |
| Catalog | `p` | show/hide the preview pane |
| Catalog | `PageUp` / `PageDown` / wheel | scroll the preview pane |
| Agent | `Ctrl+Y` | toggle focus: agent ↔ favetto |
| Agent | *(agent focus)* | every key is forwarded to the agent |
| Agent | *(favetto focus)* `Ctrl+Q` | leave the panel |
| Agent | *(favetto focus)* `Ctrl+N` | start a new session |
| Agent | *(favetto focus)* `Esc` | back to Tasks |
| Popups | `↑` / `↓` | select |
| Popups | `Enter` | confirm / next field |
| Popups | `Esc` | cancel / close |
| Text fields | `←` / `→` | move the caret |
| Text fields | `Alt+←` / `Alt+→` (or `Ctrl+←`/`Ctrl+→`) | move by word |
| Text fields | `Home` / `End` (or `Ctrl+A` / `Ctrl+E`) | start / end of line |
| Text fields | `↑` / `↓` | move the caret between lines in multiline values; otherwise previous/next field |
| Text fields | `Backspace` / `Delete` | delete before / at the caret |
| Text fields | `Alt+Backspace` / `Ctrl+W` | delete the word before the caret |
| Text fields | `Ctrl+U` / `Ctrl+K` | delete to the start / end of the line |
| Text fields | `Shift+Enter` / `Alt+Enter` | insert a newline in multiline values |
| Text fields | `Ctrl+Enter` | submit the form |
| Workflow (`w`) | `↑` / `↓` / `PageUp` / `PageDown` | scroll the graph source |
| Workflow (`w`) | `Esc` / `w` | close |
| Mouse | click the tab bar | switch tabs |
| Mouse | click a catalog folder | fold/unfold it |
| Mouse | click a catalog/task row | select; click again starts it |

`?`, `w`, `p`, and `e` are forwarded to the embedded agent while the Agent panel
has keyboard capture, and are typed literally into form/wizard fields when one is
open. `p` only toggles the preview and `e` only edits on the Catalog tab.

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

## Events panel

The **Events** tab is a selectable, scrolling list (`↑`/`↓`, `PageUp`/`PageDown`)
with a **payload side panel** that pretty-prints the selected event's JSON with
syntax highlighting.

![favetto TUI — Events tab](/screenshots/tui-events.png)

*The Events tab with the payload panel.*

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
