# favetto GPUI desktop client

Status: **task catalog (step 07)** — browse definitions grouped by repo and
folder, fuzzy-search names and prompts, read the raw Markdown source beside the
list, and launch a definition through a validated `[[vars]]` sheet. The one-shot
session is the toolbar's secondary action. The plans live in
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

On the Tasks surface, `↑`/`↓` move (Home/End and PageUp/PageDown jump), `Enter`
opens the run, `c` cancels and `r` retries a finished run (both confirm first,
then update the row immediately), `/` focuses the search field, `Esc` clears
the query, and `[`/`]` step the status filter.

On the Catalog surface, `↑`/`↓` move the definition highlight, `Enter` opens
the launch sheet, `/` focuses the search field, `⌘N`/`Ctrl+N` opens the one-shot
session sheet, and `Esc` closes the sheet, then clears the search. Inside a
sheet, `Tab`/`Shift+Tab` move between fields, `⌘↵`/`Ctrl+↵` launches (or opens
the one-shot session), and `Esc` cancels.

The Inbox and Tasks surfaces load one `tasks.list` (limit 500) and `agents.list`
snapshot on connect and follow `task.updated`, `agent.state` and `agent.event`
pushes. A blocked session shows up without a manual refresh; overlapping pushes
coalesce into one RPC round trip. A refresh keeps the previous rows visible, so
the list never flickers.

Opening a run (Enter or a row click) fills the detail column. The header stays
pinned while the tab body scrolls:

- **Output** shows the persisted `tasks.get` blob as plain monospace text, with
  a notice when the daemon truncated it. **Copy output** (and **copy** beside
  the session id) is the clipboard path, because this GPUI build has no text
  selection primitive.
- **Events** backfills `tasks.events` (up to 2000 events) and appends live
  `agent.event` pushes in `seq` order. Consecutive text/reasoning deltas become
  one block, tool start/finish events merge into one card, usage and errors keep
  their own rows. Unknown event kinds are skipped and counted, never fatal.
- **Runs** is the `tasks.runs` attempt table: attempt, status, exit code,
  duration and usage.

A blocked run pins a warning banner with the request message and its options;
**Allow once**, **Always allow** (when the agent offers it) and **Reject** map to
`agents.reply` and are disabled while the correlated reply is in flight. A
screen-detected prompt has no `request_id`, so the banner explains that it must
be answered in the agent's terminal instead of guessing.

**Follow tail** (in the tab bar) keeps the viewport at the newest content. A
wheel/trackpad scroll toward the top pauses it; when live events arrive while
pinned off the tail, the tab count and a **new events** pill appear. Clicking
the pill (or re-enabling Follow) jumps back to live. Empty Output/Events/Runs
states and per-tab Retry buttons are explicit; a failed read never hides an
older feed.

The Catalog loads one `catalog.list` on connect and whenever the daemon pushes
`catalog.updated`. Definitions are grouped by repo namespace (`repo:<name>`),
then by their folder, then by name; `Global` comes last. Group and folder
headers collapse, and both the collapsed set and the query survive the session.
The search field fuzzy-matches definition names and prompt text. Selecting a row
loads `catalog.get` and shows the raw Markdown in a monospace pane beside the
list; **Copy** puts the source on the clipboard.

**Start with variables…** (or `Enter`) opens the launch sheet. It shows the
definition's agent, provider/model, `cwd`, schedule and every declared `[[vars]]`
entry. Required values are marked and validated inline; `int`/`bool` and fixed
choice lists are checked before anything is sent. Multiline values use a wrapped
text area and accept `Enter` as a newline. A launch sends `tasks.start` with
`{ name, input }`; success closes the sheet, shows a toast and opens the new
run's detail. A daemon rejection stays attached to the sheet as an inline error,
so the user can correct the input instead of losing it.

An empty catalog explains that definitions are Markdown files under the repo's
`.favetto/tasks/` directory. **New one-shot session** (toolbar or `⌘N`/`Ctrl+N`)
opens the secondary sheet: an agent picker from `agents.list` plus an optional
working directory, sent through `tasks.start_oneshot`. The resulting run is
opened in the task detail immediately.

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
  pushed-detail flag, the shared task snapshot (tasks + sessions + agent
  roster), the Inbox selection, the Tasks filter/query/selection, the Catalog
  model, the confirmation dialog and the toast.
- `src/catalog.rs` — the pure Catalog model: repo/folder grouping, fuzzy
  name+prompt filtering, session-scoped collapse keys, preview state, and the
  launch/one-shot forms with required/type/choice validation. Unit-tested
  without a window.
- `src/inbox.rs` — the pure Inbox model: Needs you vs Recent grouping, the
  24-hour failure window, oldest-first fair-queue order, status fallbacks and
  the compact duration/cost formatters. Unit-tested without a window.
- `src/tasks.rs` — the pure Tasks model: the five-segment filter with
  query-scoped counts, fuzzy matching over name/agent/repo, newest-first and
  relevance ordering, selection resolution, and the terminal/cancellable
  helpers behind `c`/`r`. Unit-tested without a window.
- `src/detail.rs` — the pure detail model: output-envelope extraction and ANSI
  stripping, usage/cost formatting, event-feed grouping (text, reasoning,
  tools, usage, errors, boundaries), the permission-request resolver, and a
  tolerant `tasks.events` decoder that skips unknown event kinds. Unit-tested
  without a window.
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

### Step 05 deviations / additions

- **One shared snapshot.** `tasks.list(500)` + `agents.list` feed both the Inbox
  and Tasks, so the loader/command were renamed to `RefreshSnapshot` /
  `SnapshotLoaded` / `SnapshotFailed` and both surfaces share one skeleton and
  error state. This guarantees the Inbox and Tasks can never disagree about a
  row, and a push burst still costs one RPC pair rather than two.
- **Pending and cancelled rows stay under All.** The daemon's six states would
  make a six-segment control a filter maze (Hick's Law). The five required
  segments own the control; the other two keep their row label and remain
  visible under All.
- **Counts follow the search.** Each segment's badge is computed over the
  query-matched rows, so "Failed 2" always means *two rows* when Failed is
  selected, even with a query active. The All count is therefore the number of
  search matches, not the whole snapshot; the list header still shows the
  snapshot total.
- **`r` means retry on Tasks.** The generic `r` refresh binding is shadowed on
  the Tasks surface (the palette's "Refresh data" and the error Retry button
  still refresh). `c`/`r`/`/`/`[`/`]` are registered in a `Tasks` key context,
  and typed search text switches to a `Search` context, so nothing fires while
  the user is typing a query.
- **Keyboard filter stepping.** The segments are clickable and also reachable
  with `[`/`]`; the root keeps keyboard focus (like the palette input) instead of
  introducing a second focus ring that would compete with the list.
- **Optimistic rollback is guarded.** A failed cancel/retry restores the
  previous row only while the row is still exactly the optimistic snapshot, so a
  newer `task.updated` push wins over a late error response.
- **Task cost is live-session cost.** As in step 04, `tasks.list` carries no
  usage; a row shows a cost only while an `agents.list` session for that task
  reports one. Persisted per-run usage lands with the detail surface.
- **Search input scope.** Like the palette, the field accepts typed characters
  and Backspace through GPUI key events (`key_char`); full IME marked-text
  handling via `EntityInputHandler` is still deferred.
- **Live RPC check.** The typed `tasks.cancel` / `tasks.retry` request encoding
  is unit-tested. Against the running daemon, an unknown id returns a structured
  RPC error (the path behind the toast and rollback), and a scratch
  unknown-name task exercised both success paths: `tasks.retry` returned it to
  `pending`, and `tasks.cancel` then returned it `cancelled`.
- **Selection stays visible.** The Tasks list scrolls the highlight into view
  when ↑/↓, Home/End or PageUp/PageDown move it, without fighting a manual
  scroll (the scroll target only changes when the selected id does).

### Step 06 deviations / additions

- **No text selection in this GPUI build.** The pinned `gpui 0.2.2` exposes no
  selectable-text primitive, so the Output pane is monospace plain text and the
  **Copy output** action (plus **copy** beside the session id) is the clipboard
  path. This is the one place the "selectable" wording cannot be met literally
  in the current framework.
- **Run-status labels.** `RunStatus` has two outcomes the task enum does not
  (`interrupted`, `timed_out`); the Runs table renders those labels directly and
  keeps `StatusKind` for the shared icon/color vocabulary.
- **Permission banner sources.** The live `agents.list` session is authoritative
  (it carries `request_id`, options and `allow_always`). If the roster has not
  caught up, the last unresolved `InputRequested` in the feed is the fallback.
  A task whose status says `awaiting_input` but has neither source still gets a
  banner saying the prompt is not yet available, with disabled buttons.
- **Unknown event kinds are skipped in both paths.** Live `agent.event` frames
  decode tolerantly in `sse.rs`; the backfill uses a local `DetailEventsCall`
  whose result drops events this client cannot parse and reports the count.
- **Follow is viewport-driven.** A wheel/trackpad scroll up and any render that
  finds the viewport off the tail pause following; a scroll back to the bottom
  (or the pill/toggle) resumes it. Auto-scroll only reacts to content growth, so
  a deliberate scroll is never yanked back.
- **No periodic detail polling.** Opening a run loads all three parts once; a
  terminal `task.updated` triggers one reconciling reload, and retries are
  per-tab. The one-second clock only refreshes the elapsed label of a live run.
- **Attempt totals are not faked.** The wire carries `attempt` but not a
  configured retry total, so the header says `attempt n`, not `n of m`.

### Step 07 deviations / additions

- **Preview is raw source, with Copy.** As in step 06, this GPUI build has no
  selectable-text primitive, so `catalog.get` renders a read-only monospace
  pane with a **Copy** action rather than a Markdown renderer or selectable
  text.
- **Form editing is append/backspace at the end.** The launch and one-shot
  sheets reuse the shell's typed-character routing (the same model as the
  palette and Tasks search); there is no cursor movement or marked-text IME
  handling yet. Multiline vars use a wrapped, fixed-height text area and Enter
  inserts a newline; Tab/Shift+Tab move between fields.
- **Required vars block the RPC, not the button.** Submit validates every field
  and renders errors inline; the request is only sent when the form is valid.
  Optional blank values are omitted from `input`, and declared types/choices
  are coerced with the same `TaskVar` logic as the TUI.
- **Declared agents are checked against the roster.** Before sending, the
  launch sheet rejects an agent that `agents.list` marks missing or unavailable
  and shows the reason inline. The same check guards the one-shot picker. RPC
  failures still land in the sheet, never in a toast that hides the form.
- **A launch is headless.** `tasks.start` sends `interactive: null` (the daemon
  default), consistent with the other automation clients; the desktop renders
  the run's structured events instead of embedding a terminal. The embedded TUI
  remains a TUI feature.
- **Catalog refresh is coalesced.** A burst of `catalog.updated` pushes collapses
  into one `catalog.list`; the selected definition's source is invalidated and
  re-read after the list lands.
- **The launch sheet is surface-local.** It is an overlay inside the Catalog
  region rather than a window-level modal; navigating away closes it, so it can
  never own the keyboard from a hidden surface.
- **One-shot success opens the task.** The returned task is folded into the
  snapshot, its live session is added to the roster immediately, and the detail
  page opens without waiting for the next `agents.list` poll.
