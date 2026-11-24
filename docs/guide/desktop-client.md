# GPUI desktop client

favetto ships two clients:

| Client | Location | Transport |
|--------|----------|-----------|
| **TUI** | `crates/favetto-tui` | Unix socket (local) or WebSocket (remote) |
| **GPUI desktop** | `gpui/` | HTTP `POST /rpc` + SSE `GET /events` |

These are the only supported clients. A native Android app is future work.

The desktop client is a native GUI built with [GPUI](https://gpui.rs/), the
GPU-accelerated Rust UI framework behind Zed. It talks to the same daemon as the
TUI and shows the same vocabulary: Inbox, Tasks, Agent, Catalog, Workflows,
Schedules and Settings.

## Prerequisites

- **Rust**: `rustc 1.94` or newer.
- **Linux**:
  - `libxkbcommon` (`libxkbcommon-dev` on apt, `libxkbcommon-devel` on dnf);
  - Wayland or X11 development headers (`libwayland-dev` or `libx11-dev`);
  - fontconfig (`fc-list`/`fc-match`).
- A running Wayland or X11 display server.

## Build and run

```bash
cd gpui
cargo run
```

The client opens a window titled **favetto**. Start the daemon first:

```bash
favetto daemon
```

The top bar shows the active repo and the connection status. Settings shows the
endpoint, the token source and the daemon's working directory.

## Connect to a daemon

Without configuration the client connects to `http://127.0.0.1:7878` and reads
the shared bearer token from `~/.local/share/favetto/token` (the same file the
TUI uses).

Override either value in `~/.config/favetto/gpui.toml`:

```toml
# Optional. Accepts host:port, http(s)://host, or a full /rpc URL.
endpoint = "127.0.0.1:7878"
# Optional. Overrides the shared token file.
# token = "…"
# Optional: "system" (default), "dark" or "light".
theme = "system"
# Optional: "comfortable" (default) or "compact".
density = "comfortable"
```

Endpoint input is normalized: trailing slashes, a pasted `/rpc` or `/events`
path, and a missing scheme are accepted. The same endpoint works through a
TLS-terminating proxy; see [Remote access](./remote-access).

The Settings surface (**⌘,** / **Ctrl+,**) edits the endpoint and token in
place, can test the connection with `system.ping`, and switches theme and
density. **Save** rewrites `gpui.toml` and reconnects.

Settings also edits the **daemon's global config** (`config.toml`) through
`config.get`/`config.set` as a schema-backed form. Every documented section
(`agent`, `agents`, `daemon`, `executor`, `git`, `auth`, `repos`, `mcp`) has
labelled controls: text/path inputs, checkboxes, validated integers with units,
enum chips, repeatable list rows, key/value map rows, and add/remove cards for
agents, repository seeds and MCP servers. Defaults and field descriptions come
from the generated schema, optional values can be reset to their default, and a
missing file opens with defaults. `[tui]` is labelled client-side and lives in
the advanced view.

The **Advanced / raw TOML** disclosure keeps the repair path: comments, unknown
keys and a malformed file are edited as text without blocking Settings. Form
edits are patched into the loaded document, so comments, unknown keys and the
file mode survive. Saving validates syntax and the daemon's startup preflight
(agent registry, git signing resolution, MCP seeds) before an atomic write, and a
stale revision is rejected rather than clobbering another client's edit.

Secret values (MCP `env`/`headers`, agent `env`) never leave the daemon:
`config.get` replaces them with `__FAVETTO_REDACTED__`. The form masks them,
starts an untouched secret in an "unchanged" state, and echoes the sentinel back
on save so the daemon restores the literal value from disk; **Clear** removes the
key explicitly. Daemon preflight errors are attached to the named field when the
message starts with its dotted path and remain in the card-level summary.

Most sections are read once at startup, so a successful save reports
`restart_required`; `[tui]` is the exception and is labelled as client-side.
`FAVETTO_*` environment overrides are shown per field and in the card summary
because the effective value can differ from the saved file. Live repository and
MCP changes belong to the `repos.*`/`mcp.*` RPCs; the config sections are only
startup seeds.

## Common shortcuts

| Shortcut | Action |
|----------|--------|
| `⌘K` / `Ctrl+K` | Command palette |
| `g i`, `g t`, `g a`, `g c`, `g w`, `g s` | Jump to Inbox, Tasks, Agent, Catalog, Workflows, Schedules |
| `⌘,` / `Ctrl+,` | Settings |
| `?` | Shortcut sheet |
| `Esc` | Close the topmost overlay or step back |

Tasks, Agent, Catalog and Workflows have surface-local keys for search, cancel,
retry, start, interrupt and launch; the shortcut sheet lists them all.

### Agent panel

**Agent** (`g a`) is a free-standing ACP conversation, not tied to a task or
run. It resolves the active repository from the top bar and starts `agents.start`
with that repository's main checkout as `cwd`; it never falls back to the daemon
cwd or a task worktree. With `repo: global` active, the panel asks you to select
a repository instead of starting somewhere unexpected.

Pick an available agent, optionally type a model override (a bare id or
`provider/model`), and press **Start session** (`n` also starts a fresh
session). The header freezes the start cwd; if you switch repositories while a
session is live, the existing session keeps its original cwd — start a new one
for the new checkout. **i** interrupts the current turn, **x** closes the
session through `agents.close`, and **m** focuses the same slash-command
composer as the task detail. Model, thinking level and build/plan mode appear in
the shared session strip whenever the agent advertises them.

**Sessions** (`s`) opens the on-demand session switcher. It lists live/retained
sessions first, then durable Favetto history newest first, without showing the
raw session id. A row shows the agent/title, lifecycle (`running`, `awaiting
input`, `ended`, `loading`, `failed` or `cannot load`), cwd and any linked task;
the current session is marked and cannot be selected. Choosing a live row sends
`agents.attach`, choosing an ended loadable row sends `agents.load`, and failed
or unsupported rows explain why they are disabled. Selecting a row never starts
or closes a session: **New session** remains the only action that replaces the
conversation. The panel keeps a bounded per-session conversation cache, so
switching away and back preserves that session's feed, activity and composer
draft. `↑`/`↓` move the highlight, `Enter` opens the row and `Esc` closes the
menu.

A finished run's detail header also offers **Delete run**, which permanently
removes the run, its recorded attempts and its dependency edges after a
confirmation that names the run. It is only shown for `succeeded`, `failed` and
`cancelled` runs; the daemon rejects deletion of a `pending`, `running` or
`awaiting_input` run. Deleting removes that run's usage history, cleans up its
worktree and branch when one is tracked, and never deletes the agent provider's
own transcript.

## Development

Run the client's gates from its standalone workspace:

```bash
cd gpui
cargo fmt --check
cargo clippy -- -D warnings
cargo test
```

The full feature walkthrough, architecture notes and known framework limits are
in [`gpui/README.md`](https://github.com/oknozor/favetto/blob/main/gpui/README.md)
and [`docs/design/gpui-client.md`](../design/gpui-client).
