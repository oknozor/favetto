# favetto GPUI desktop client

Status: **inbox (step 04)** — the landing surface now answers "What needs
me now?": blocked sessions and failures from the last 24 hours in **Needs you**
(oldest waiting first, accent stripe + icon), the newest runs in **Recent**, a
first-fetch skeleton, an error state with Retry, and live refreshes driven by
`task.updated`, `agent.state` and `agent.event` pushes. The plans live in
`docs/design/gpui-client.md`.

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
the daemon running locally, the top bar shows the repo name and the connection
pill (**● Connected**), and the status bar shows the endpoint, ping round-trip
and live event cursor. Stopping the daemon flips the pill to
**◌ Reconnecting (n)**; restarting recovers automatically. A rejected token
shows **● Unauthorized** together with the token file path to fix, and the
Reconnect action retries after the token/test daemon is corrected.

Keyboard: `⌘K`/`Ctrl+K` opens the command palette, `g i`/`g t`/`g c`/`g w`/
`g s` jump to Inbox/Tasks/Catalog/Workflows/Schedules, `⌘,`/`Ctrl+,` opens
Settings, `?` opens the shortcut sheet, `Esc` closes the topmost overlay or
steps back from a pushed detail. On the Inbox, `↑`/`↓` move the Needs you
highlight, `Enter` opens the highlighted run, and `r` refreshes the surface.

The Inbox loads `tasks.list` and `agents.list` on connect and follows
`task.updated`, `agent.state` and `agent.event` pushes. A blocked session shows
up without a manual refresh; overlapping pushes coalesce into one RPC round trip.

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

## Architecture notes

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
  folds updates in and calls `cx.notify()`. It also owns the active route, the
  pushed-detail flag, the Inbox snapshot (tasks + sessions), its selection and
  the selected run.
- `src/inbox.rs` — the pure Inbox model: Needs you vs Recent grouping, the
  24-hour failure window, oldest-first fair-queue order, status fallbacks and
  the compact duration/cost formatters. Unit-tested without a window.
- `src/routes.rs` — the route vocabulary, labels, icons, jump hints and empty
  copy in one place.
- `src/actions.rs` — actions plus the keymap; every shortcut is bound to an
  action in a `key_context`, never matched as a raw key string in a view.
- `src/palette.rs` — pure command-palette model: fuzzy filtering, wrapping
  selection, recents and command execution kinds (unit-tested without a window).
- `src/layout.rs` — the responsive breakpoint helper (the container-query
  equivalent for this GPUI version).
- `src/components.rs` — focus ring, keycaps, connection pill, empty state and
  shortcut rows shared by the shell and route views.
- `src/views/` — one entity per route plus the contextual detail view.

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

### Step 03 deviations / additions

- **No container queries in this GPUI build.** The pinned `gpui 0.2.2` exposes
  no `container_query` primitive, so `layout::container_query` reads the
  full-window viewport width behind one named function. For this shell the root
  container is the window, so the result is equivalent; the breakpoint is
  `theme::NARROW_BREAKPOINT` (1120 px). Below it the detail column becomes a
  pushed page with a Back button.
- **Settings is read-only in this step.** It shows the effective endpoint, token
  source, live connection pill, appearance and a Reconnect action; editing and
  the agent roster land with step 09.
- **Palette input scope.** The palette accepts typed characters and Backspace
  through GPUI key events (`key_char`); full IME marked-text handling via
  `EntityInputHandler` is deferred until search needs non-ASCII composition.
  The palette remembers its last query for the session and keeps five recent
  commands in memory (newest first).
- **Inbox badge wiring.** `ClientState::awaiting_count` and the badge rendering
  path are live; the inbox feed that increments it arrives in step 04.
- **Extra Settings chord.** `⌘,`/`Ctrl+,` opens Settings in addition to the
  required `g` jumps and the palette, because `g s` belongs to Schedules.

### Step 04 deviations / additions

- **Recent excludes Needs you rows.** A blocked or recently failed run is
  already in the first group; repeating it one line below would make the
  landing surface look busier than it is.
- **Usage cost comes from live sessions.** `tasks.list` omits the run output and
  the task row carries no usage, so a Recent row shows a cost only while an
  `agents.list` session for that task reports `cost_usd`. The detail surface
  (step 06) owns persisted per-run usage.
- **Elapsed waiting time is the run clock.** The wire exposes `started_at` but
  not when a prompt first appeared, so the fair-queue order and the elapsed cell
  use `started_at`, falling back to `created_at`.
- **Task detail is still a placeholder.** `Enter` (or a row click) selects the
  run, pushes the detail page on narrow windows and echoes the selected name in
  the wide detail column; the header, tabs and permission controls land in
  step 06 exactly as planned.
- **First-fetch error coverage.** An unauthorized/disconnected transport that
  never reaches `Connected` transitions the Inbox from skeleton to the error
  state with Retry, instead of waiting forever for a first fetch that cannot run.
