# favetto GPUI desktop client

Status: **transport (step 02)** — the standalone workspace from step 01 now has
a real daemon connection: MessagePack RPC over `POST /rpc`, the authenticated
SSE stream over `GET /events` with `Last-Event-ID` resume, connection settings
from `~/.config/favetto/gpui.toml`, and a `ClientState` entity the shell renders
in a temporary status line. The navigation surfaces land in steps 03+; the plans
live in `docs/design/gpui-client.md`.

## What it is

A native desktop client for a running favetto daemon. GPUI is the GPU
accelerated Rust UI framework the Zed editor is built on. The client mirrors the
daemon's vocabulary (`Inbox → Tasks → Catalog → Workflows → Schedules →
Settings`) so users can move between the TUI, the web client and this app
without relearning anything (docs/design/gpui-client.md §2).

## Prerequisites

- **Rust toolchain**: `rustc 1.94` or newer (package `rust-version`).
- **Linux**: `libxkbcommon` (`libxkbcommon-dev` on apt, `libxkbcommon-devel`
  on dnf) — the UI framework needs it for keyboard handling at runtime.
- **Wayland or X11**: the build features of `gpui` target Wayland or X11
  (`libwayland-dev` or `libx11-dev` respectively).
- **fontconfig**: `fc-list`/`fc-match` (`libfontconfig-dev`/`libfontconfig1-dev`)
  — the text system uses fontconfig on Linux.

A display server running (Wayland or X11) is needed to actually open the
window.

## Building and running

```sh
cargo run
```

The window opens titled **"favetto"** (default 1280×800, minimum 900×600). With
the daemon running locally, the status bar shows **Connected**, the ping
round-trip cwd/latency, and the live event cursor. Stopping the daemon flips it
to **Reconnecting**; restarting recovers automatically. A rejected token shows
**Unauthorized** together with the token file path to fix, and the Reconnect
button retries after the token/test daemon is corrected.

## Pointing it at a daemon

The connection is configured in `~/.config/favetto/gpui.toml`:

```toml
# Optional. Accepts host:port, http(s)://host, or a full /rpc URL.
endpoint = "127.0.0.1:7878"
# Optional. Overrides the shared token file.
# token = "…"
```

Without a file the client uses `http://127.0.0.1:7878` and the token at
`~/.local/share/favetto/token` (the same file the TUI and web client use).
Endpoint input is normalized (Postel's Law): trailing slashes, a pasted `/rpc`
or `/events` path, and a missing scheme are all accepted.

## Architecture notes for this step

- `src/config.rs` — config/token loading and endpoint normalization.
- `src/transport.rs` — typed `RpcClient` over `favetto-wire` frames, bearer auth,
  and one wrapper per method used by the client (`system.ping`, tasks, catalog,
  agents, workflows, schedules, `events.subscribe`). 401 becomes
  `ClientError::Unauthorized`; malformed frames become typed protocol errors.
- `src/sse.rs` — incremental SSE parser (id/event/data/retry, CRLF, multi-line
  data, UTF-8 split across chunks), monotonic cursor, exponential backoff, and a
  deliberate stop on 401. Frames decode into a typed `Push` value.
- `src/runtime.rs` — a dedicated tokio thread bridged to GPUI with
  `async-channel`: commands in, connection state/cursor/pushes out.
- `src/state.rs` — the single `ClientState` entity; the foreground drain task
  folds updates in and calls `cx.notify()`.

## Development

```sh
cargo fmt --check
cargo clippy -- -D warnings
cargo test
```

Deviations from the plan are recorded in this file as the chain progresses
(docs/design/gpui-client.md §5, cross-cutting constraint 4).

### Step 02 deviations / additions

- `config::save_to` exists already (rather than waiting for step 09) so the file
  format has one owner beside `load_from`; Settings will call it.
- The status line also shows the resume cursor and the last decoded push. This is
  temporary diagnostic surface, replaced by the real status bar in step 03.
- `RpcClient` exposes the full set of step-02 required wrappers but does not yet
  have UI call sites for all of them; the module carries a scoped
  `#![allow(dead_code)]` until the views arrive.
