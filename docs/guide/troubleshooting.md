# Troubleshooting

## A task will not start

The daemon reports a task with no agent as soon as it is started. There are two
common causes:

- **No default agent and no `agent` header.** A task must name an agent or
  inherit `[agent].default`. Set one in `~/.config/favetto/config.toml`:

  ```toml
  [agent]
  default = "opencode"
  ```

- **The agent binary is not installed.** The agent is still listed but marked
  `available = false`, and launching it fails with a clear error. Install the CLI
  or point `command` at the right executable. Availability is resolved once at
  daemon startup, so restart the daemon after changing your `PATH`.

An agent that does not speak ACP (or whose `command`/`args` do not start its ACP
server mode) fails at `initialize`; the task records the protocol error. Check
the agent's documentation for its ACP mode (for example `opencode acp`).

## An agent commit hangs (signed commits)

If an agent's `git commit` stalls waiting for a pinentry or askpass prompt, your
git config has `commit.gpgsign = true`. favetto's default `[git] signing = "off"`
injects `commit.gpgsign = false` and prevents this. If you opted into `ssh` or
`gpg` signing, use a passphrase-less key (or one held by `ssh-agent`), or provide
a passphrase source. See [Git and commit signing](./git-signing).

If a prompt does appear, the task no longer silently stays `running`: it flips to
**awaiting input**, the TUI plays the attention cue, and a `task_awaiting_input`
event fires. Select the task and press **Enter** to open the Agent panel and
answer the prompt in the live session. The task returns to `running` once the
agent continues. Detection is configurable under `[executor]`
(`detect_awaiting_input`, `awaiting_input_quiet_ms`).

## A catalog edit seems ignored

If a file fails to parse, the daemon keeps the **previous** definition instead of
dropping the task, so a half-written edit never removes a task. Watch the daemon
log for `skipping invalid task definition; keeping previous definition`, then fix
the TOML and save again. An invalid `[[vars]]` declaration (empty name, a
duplicate, `_prev`, or an empty prompt) also makes the whole file fail to parse.

The catalog reloads with a 200 ms debounce. A task that was already queued picks
up the freshly loaded definition when it starts; a run already in flight keeps
the definition it was dispatched with.

## A remote TUI cannot connect

- Check the daemon is listening on the address you dialed (`--listen`) and that
  it is reachable.
- Make sure you passed a valid token (`--token-file`), a username/password
  (`--user`/`--password`), or a fresh pairing code (`--pair-code`). Pairing
  codes expire after 60 seconds and are single-use.
- A rejected token returns the RPC error code `-32001`; a rejected login returns
  HTTP 401 (and 429 after too many failures).

See [Remote access](./remote-access).

## Sound does not play

The TUI plays cues on the **client** machine. Run
`favetto tui --test-sound` to play every configured cue once and print the
resolved player. If no player is found on `PATH`, favetto falls back to the
terminal BEL. Precedence is CLI flag → `FAVETTO_SOUND*` env var → `[tui.sound]` →
built-in default. See [TUI](./tui#sound-notifications).
