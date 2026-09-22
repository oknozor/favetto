# Getting started

favetto is a single binary that runs as a **daemon** (scheduler, task queue,
persistence, remote API) and a **TUI client**. This page gets both running and
attaches the client, locally or remotely.

## Prerequisites

- A Rust toolchain (edition 2021, MSRV 1.80) to build favetto.
- At least one external coding-agent CLI (opencode, Claude Code, pi, Mistral
  Vibe, …) installed and configured — see [Embedded agents](./agents).

## Build

From the repository root:

```bash
cargo build
```

## Run the daemon

```bash
# terminal 1 — the daemon
./target/debug/favetto daemon
```

## Attach the TUI

```bash
# terminal 2 — the TUI (local attach over the unix socket)
./target/debug/favetto tui

# or attach remotely over WebSocket (token auto-created in ~/.local/share/favetto/token)
./target/debug/favetto tui --remote ws://127.0.0.1:7878 \
    --token-file ~/.local/share/favetto/token

# configure external agents, then start tasks from the TUI
```

The daemon exposes one wire protocol over two transports — a Unix socket for
local attach and a token-authenticated WebSocket for remote attach. See the
[Remote API reference](/reference/remote-api) for the method list.

## Appearance

The TUI ships a built-in dark and light style that is selected automatically
from the terminal background (an OSC 11 query, then `COLORFGBG`, then dark). Set
`FAVETTO_THEME=dark` or `FAVETTO_THEME=light` to override it. There is no theme
file or picker.

## Next steps

- [Tasks & catalog](./tasks) — write declarative tasks with TOML headers.
- [Configuration](./configuration) — agents, daemon defaults, sound.
- [TUI](./tui) — sound notifications, the Ctrl+P menu, and help.
- [Webhooks & hooks](./webhooks) — trigger tasks from GitHub events.
