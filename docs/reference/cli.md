# CLI reference

Generated from `crates/favetto/src/cli.rs` by the docs generator. Do not edit by hand.

`favetto` is the daemon and doc generator; its `tui` subcommand execs the sibling `favetto-tui` client binary. Run `favetto <command> --help` for the same information at the terminal.

## `favetto daemon`

Run the favetto daemon (event bus, API server, scheduler, executor)

| Flag | Value | Default | Description |
|------|-------|---------|-------------|
| `--config` | `<CONFIG>` | — | Path to the config file (default `~/.config/favetto/config.toml`) |
| `--socket` | `<SOCKET>` | — | Path to the Unix domain socket for local TUI attach |
| `--listen` | `<LISTEN>` | — | TCP listen address for the WebSocket API (loopback by default) |
| `--data-dir` | `<DATA_DIR>` | — | Directory for SQLite + token (default `~/.local/share/favetto`) |
| `--tasks-dir` | `<TASKS_DIR>` | — | Directory of task-definition `.md` files (the task catalog) |

## `favetto tui`

Attach the TUI to a running daemon (local Unix socket or remote WebSocket)

| Flag | Value | Default | Description |
|------|-------|---------|-------------|
| `--remote` | `<REMOTE>` | — | Remote WebSocket URL (`ws://...`). If unset, attach to the local Unix socket |
| `--token-file` | `<TOKEN_FILE>` | — | Bearer token file for remote auth (defaults to `<data_dir>/token`) |
| `--socket` | `<SOCKET>` | — | Unix socket path (overrides the default) |
| `--pair-code` | `<PAIR_CODE>` | — | Short-lived pairing code to exchange for a token (remote attach only) |
| `--config` | `<CONFIG>` | — | Path to the client-local config file (default `~/.config/favetto/config.toml`) |
| `--sound` |  | — | Force sound on (overrides config and env) |
| `--no-sound` |  | — | Disable sound (overrides config and env) |
| `--sound-command` | `<SOUND_COMMAND>` | — | Custom player command; `{file}` is the sound path (implies player = "command") |
| `--test-sound` |  | — | Play every configured cue once, print the resolved player, and exit |

## `favetto pair`

Print a short-lived pairing code for remote TUI attachment

| Flag | Value | Default | Description |
|------|-------|---------|-------------|
| `--url` | `<URL>` | `http://127.0.0.1:7878` | Daemon HTTP base URL (e.g. http://127.0.0.1:7878) |

## `favetto token-rotate`

Rotate the bearer token

| Flag | Value | Default | Description |
|------|-------|---------|-------------|
| `--data-dir` | `<DATA_DIR>` | — | Directory for the token file |

