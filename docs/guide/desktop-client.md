# GPUI desktop client

favetto ships two clients:

| Client | Location | Transport |
|--------|----------|-----------|
| **TUI** | `crates/favetto-tui` | Unix socket (local) or WebSocket (remote) |
| **GPUI desktop** | `gpui/` | HTTP `POST /rpc` + SSE `GET /events` |

These are the only supported clients. A native Android app is future work.

The desktop client is a native GUI built with [GPUI](https://gpui.rs/), the
GPU-accelerated Rust UI framework behind Zed. It talks to the same daemon as the
TUI and shows the same vocabulary: Inbox, Tasks, Catalog, Workflows, Schedules
and Settings.

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

## Common shortcuts

| Shortcut | Action |
|----------|--------|
| `⌘K` / `Ctrl+K` | Command palette |
| `g i`, `g t`, `g c`, `g w`, `g s` | Jump to Inbox, Tasks, Catalog, Workflows, Schedules |
| `⌘,` / `Ctrl+,` | Settings |
| `?` | Shortcut sheet |
| `Esc` | Close the topmost overlay or step back |

Tasks, Catalog and Workflows have surface-local keys for search, cancel, retry
and launch; the shortcut sheet lists them all.

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
