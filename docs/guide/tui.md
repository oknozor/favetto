# TUI

This page covers the client-side TUI behaviors: theme, sound notifications, the
Ctrl+P menu, help, and one-shot tasks. For the Agent panel's focus and mouse
handling see [Embedded agents](./agents).

## Tasks list

The **Tasks** tab lists recent runs with their status, age, and error. Once a
headless run finishes, the agent's own session title (for opencode, from
`session list --format json`) is shown in a truncated `SESSION` column, so
repeated runs of the same catalog task stay distinguishable. The cell is blank
for agents that do not report titles, or until the title becomes available.

## Theme

The TUI ships a built-in dark and light style that is selected automatically from
the terminal background (an OSC 11 query, then `COLORFGBG`, then dark). Set
`FAVETTO_THEME=dark` or `FAVETTO_THEME=light` to override it. There is no theme
file or picker.

## Sound notifications

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

Press **w** to open a floating workflow graph overlay listing the catalog's
`needs`/`spawn` edges as Graphviz DOT (also persisted to
`<data_dir>/workflow.dot`). Press **w** or **Esc** to close it, and scroll it with
**↑**/**↓** or **PageUp**/**PageDown**. It refreshes while open when the catalog
changes. `w` is likewise forwarded to a capturing agent and typed literally into
form fields.

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
