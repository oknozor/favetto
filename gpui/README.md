# favetto GPUI desktop client

Status: **step 10 (polish)** — the client now has an accessibility and
state-coverage pass. Every surface has loading, empty, error and unauthorized
states; errors carry an actionable message plus a **Copy error** control; Tab
traverses the real tab stops in visual order with a visible focus ring; all
targets meet the 32 px floor (40 px for primary actions); long task and event
lists are virtualized; the live event feed is capped; and the one decorative
animation (the toast fade) respects the OS reduced-motion setting. The plans
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
the daemon running locally, the top bar shows the repo name and the connection
pill (**● Connected**), and the status bar shows the endpoint, ping round-trip
and live event cursor. Stopping the daemon flips the pill to
**◌ Reconnecting (n)**; restarting recovers automatically. A rejected token
shows **● Unauthorized** together with the token file path to fix, and the
Reconnect action retries after the token/test daemon is corrected.

Keyboard: `⌘K`/`Ctrl+K` opens the command palette, `g i`/`g t`/`g c`/`g w`/
`g s` jump to Inbox/Tasks/Catalog/Workflows/Schedules, `⌘,`/`Ctrl+,` opens
Settings, `?` opens the shortcut sheet, `Esc` closes the topmost overlay or
steps back from a pushed detail. **Tab** and **Shift+Tab** move real focus
through the top bar, sidebar, active surface, detail column (Inbox and Tasks
only) and status bar in visual order; **Enter** or **Space** activates the
focused control. On the Inbox, `↑`/`↓` move the Needs you highlight, `Enter`
opens the highlighted run, and `r` refreshes the surface.

On the Tasks surface, `↑`/`↓` move (Home/End and PageUp/PageDown jump), `Enter`
opens the run, `c` cancels and `r` retries a finished run (both confirm first,
then update the row immediately), `/` focuses the search field, `Esc` clears
the query, and `[`/`]` step the status filter.

On the Catalog surface, `↑`/`↓` move the definition highlight, `Enter` opens
the launch sheet, `/` focuses the search field, `h` collapses/expands the
highlighted folder, `⌘N`/`Ctrl+N` opens the one-shot session sheet, and `Esc`
closes the sheet, then clears the search. Inside a sheet, `Tab`/`Shift+Tab`
move between fields, `←`/`→` cycle a declared choice list, `⌘↵`/`Ctrl+↵`
launches (or opens the one-shot session), and `Esc` cancels.

The Inbox and Tasks surfaces load one `tasks.list` (limit 500) and `agents.list`
snapshot on connect and follow `task.updated`, `agent.state` and `agent.event`
pushes. A blocked session shows up without a manual refresh; overlapping pushes
coalesce into one RPC round trip. A refresh keeps the previous rows visible, so
the list never flickers.

The Inbox grouping and the filtered Tasks list are memoized on the snapshot
revision plus the search/filter, and handed to the views as `Rc` values, so a
render never clones or re-sorts the 500-row snapshot. The Tasks table is a
`uniform_list`: only the visible row window is measured and painted. The
detail's Events tab uses GPUI's variable-height `list` with the same lazy
window, and the live feed is capped at 2,000 retained frames.

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

The Workflows surface is reached with `g w` or from a run's **View workflow**
button. It groups the current `tasks.list` snapshot by `root_or_self` and shows
each root's `done / total` and a progress bar (counted from the instances the
daemon returns, never estimated). Selecting a root loads `workflow.inspect` and
renders its five buckets — **Ready**, **Running**, **Failed**, **Blocked** and
**Terminal** — with status, attempt and the predecessor each instance waited
for. A directly started run is its own root and carries a short explanation;
when the snapshot has no roots at all the surface explains where roots come
from instead of showing a blank panel.

`↑`/`↓` move the instance highlight, `↵` opens the instance in the task detail,
`c` opens the root-cancel confirmation (it names the number of non-terminal
instances) and `r` retries the highlighted failed instance. `[`/`]` move the
root highlight without a mouse. The same actions are available as buttons;
**Refresh** (or the palette's Refresh data) reloads the inspect and the DAG
source. Cancel marks every non-terminal row immediately and
retry moves its row to Ready, then both reconcile with the RPC response and the
next `task.updated` push. `workflow.get` is shown as readable DOT in a monospace
panel with a one-line edge legend and **Copy DOT** (no Graphviz binary is
embedded).

The Schedules surface (`g s`) lists `schedules.list` as a table of enabled
state, cron expression, task and the daemon-computed next run (absolute UTC plus
a relative hint). `n` opens the create sheet, `↵` edits the highlighted row and
`d` opens the delete confirmation. The sheet has a cron field with inline
validation and a plain-language hint (five-field expressions get a zero-seconds
field; the hint names the cadence and the next fire), a task picker from
`catalog.list`, and an enabled toggle (`⌘E`/`Ctrl+E` or a click). `Tab`/
`Shift+Tab` walk the fields; when the task picker is focused, `↑`/`↓` move its
selection. `⌘↵`/`Ctrl+↵` saves. A rejected save stays attached to the sheet; a
saved row appears immediately with its `next_run`.
Deleting names the task and cadence, and the row disappears only after the
daemon answers.

Settings (`⌘,`/`Ctrl+,`) is stacked into four cards. **Connection** edits the
daemon address and token in place: `host:port`, `http(s)://…` and full `/rpc`
URLs are accepted, the normalized base URL is previewed before saving, and a bad
address shows the precise reason without touching the live connection. **Test
connection** runs `system.ping` against the draft and reports the round-trip
latency and daemon cwd, or the exact transport error. **Save** rewrites
`gpui.toml` and restarts the transport; **Use defaults** restores
`http://127.0.0.1:7878` and the shared token file. **Agents** is the read-only
`agents.list` roster (availability, command, `acp`/`pty` backend, live sessions)
plus **Past sessions** from `agents.history`: active, ended, loading, failed and
cannot-load rows are labelled, and a loadable row restores the provider's
conversation through `agents.load` by Favetto-local id. A load never starts a
fresh session; progress and errors are shown inline and as a toast, and the
replayed transcript appears under the row only after the load succeeds (a
bounded preview; the full conversation stays with the agent).
**Appearance** switches System/Dark/Light and Comfortable/Compact live and
persists both. **About** shows the client version, daemon cwd, endpoint, config
path and a docs link, and states that no telemetry is collected.
`Tab`/`Shift+Tab` walk the controls and `Enter` activates the focused one;
`⌘↵` saves, `⌘T` tests.

## Pointing it at a daemon

The connection is configured in `~/.config/favetto/gpui.toml`:

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

Without a file the client uses `http://127.0.0.1:7878` and the token at
`~/.local/share/favetto/token` (the same file the TUI and web client use).
Endpoint input is normalized (Postel's Law): trailing slashes, a pasted `/rpc`
or `/events` path, and a missing scheme are all accepted. An address that cannot
name a host is refused with the specific reason in Settings rather than silently
replaced. A token typed into Settings is written as an override; leaving the
field blank keeps the shared token file.

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
  model, the confirmation dialog and the toast. Inbox/Tasks derivations are
  memoized on a snapshot revision and shared with `Rc`; action errors use one
  `copy_text` clipboard path.
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
- `src/workflow.rs` — the pure Workflows model: roots grouped by
  `root_or_self`, the five runtime buckets, dependency labels joined from the
  cached task rows, `done / total` progress, optimistic cancel/retry state and
  the `workflow.get` source model. Unit-tested without a window.
- `src/schedules.rs` — the pure Schedules model: cron normalization/validation
  with the daemon's parser configuration, plain-language cadence and next-fire
  hints, the sorted table, the create/edit sheet and the delete confirmation.
  Unit-tested without a window.
- `src/sessions.rs` — the pure persisted-session model: the `agents.history`
  snapshot, load states (active/loading/ready/failed/cannot-load), load by
  Favetto-local id, and the restored transcript preview. Unit-tested without a
  window.
- `src/settings.rs` — the pure Settings model: the endpoint/token draft, the
  precise normalization preview, the connection-test result and the logical
  Tab order. Unit-tested without a window.
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
- `src/motion.rs` — OS reduced-motion detection (GTK settings plus
  `FAVETTO_REDUCE_MOTION`/`GTK_ENABLE_ANIMATIONS`). The only decorative motion
  is the toast fade; everything else changes state immediately.
- `src/components.rs` — focus ring, keycaps, connection pill, empty state,
  copy-error action and shortcut rows shared by the shell and route views. The
  focus helper enforces the 32 px minimum target.
- `src/views/` — one entity per route plus the contextual detail view. Tasks
  and the detail Events tab are virtualized.

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

### Step 08 deviations / additions

- **Roots come from the shared snapshot.** There is no `workflow.list` RPC, so
  the Workflows list groups the same `tasks.list` rows the Inbox and Tasks use
  by `root_or_self`, newest root first. A root whose root task fell outside the
  500-row window keeps working and is named after its first visible instance.
- **Five buckets, one terminal section.** `workflow.inspect` returns `ready`,
  `running`, `failed` and `blocked`; the terminal section is the remaining
  succeeded/cancelled instances, in inspect order. Empty buckets still render
  their explanation and a `0`, so counts and rows cannot disagree.
- **Done means succeeded.** `done / total` and the progress bar count only
  `succeeded` instances. A failed or cancelled root therefore never shows a
  full bar even though every row is terminal; totals are never estimated.
- **Dependencies join through `tasks.list`.** `workflow.inspect` omits
  `parent_id`, so each row's predecessor label is joined from the cached task
  rows. A fresh snapshot re-joins the open root locally instead of issuing
  another inspect RPC.
- **DAG source, not Graphviz.** `workflow.get` is a catalog-wide DOT string, so
  it is rendered as readable monospace source beside the runtime view with a
  short spawn/needs/join legend, **Copy DOT** (no text selection in this GPUI
  build) and a collapse toggle. No Graphviz binary or DOT parser is embedded.
- **Only failed instances retry from Workflows.** `r` and the row button call
  `workflow.retry` for a `failed` row; succeeded/cancelled rows stay retryable
  from the Tasks surface, which already has the broader terminal rule.
- **Cancel is root-scoped and counted.** `c` (or **Cancel root**) opens a
  confirmation naming the exact non-terminal count. The optimistic write marks
  every non-terminal row terminal; a rejected RPC restores the previous detail,
  and the response's cancelled ids reconcile with any newer push.
- **Live refresh is coalesced.** A `task.updated` for a task in the open root
  folds the row immediately, then marks the view dirty so overlapping pushes
  collapse into one follow-up `workflow.inspect`. The highlight follows the
  instance id and survives bucket moves.
- **Direct starts explain themselves.** A run whose snapshot row has no
  `root_id` is its own root (`root_or_self`); the surface keeps its single
  instance visible and adds a banner explaining that spawned children appear
  there. A snapshot with no roots shows the route's empty state, never a blank
  panel.
- **No embedded root graph renderer.** The runtime inspect is a structured row
  view rather than a drawn DAG; the required catalog DAG is the DOT source
  panel. A layout engine can be added behind the same `WorkflowGraph` model
  later without changing the RPCs.

### Step 09 deviations / additions

- **Cron validation matches the daemon.** `schedules.upsert` is registered with
  a `Seconds::Required` parser (`dom_and_dow(true)`, UTC), so the client uses
  the same configuration via `croner`. A five-field expression is accepted as a
  convenience and normalized by inserting a zero-seconds field before the RPC,
  so a saved schedule always fires on the cadence the hint describes.
- **Density is a spacing scale, not a second layout.** Comfortable uses the
  documented 4/8/12/16/24 scale; Compact uses 3/6/9/12/18 everywhere the theme
  tokens are read. Fixed row/control heights stay as designed so targets never
  drop below the accessibility minimum (docs §2, Fitts's Law).
- **Theme/density live in `gpui.toml`.** The file now carries optional `theme`
  and `density` keys beside `endpoint`/`token`. `System` follows the OS; Dark
  and Light override it. Changing either applies on the next render and writes
  the whole settings file, so an appearance edit never drops the connection
  override (and a token sourced from the shared file is never copied into it).
- **The daemon version is not reported.** `system.ping` carries `pong` and
  `cwd` only; there is no `system.version`/info method in the wire API. The
  About card therefore shows the client version, the daemon cwd and endpoint,
  and says the daemon version is not reported rather than inventing one.
- **A connection edit restarts the transport.** The runtime owns the SSE URL and
  RPC client for one `ClientConfig`, so saving a new endpoint (or using defaults)
  joins the old worker and starts a fresh one from `Connecting`. A validation or
  file-write failure returns before that point, leaving the old connection live.
- **A test uses a throwaway client.** "Test connection" builds a temporary
  `RpcClient` for the draft endpoint/token and pings it; a failure never flips
  the live status pill to Unauthorized/Disconnected.
- **The schedule sheet owns typed text.** While it is open the key context adds
  `Sheet Search`, so spaces in a cron expression and letters such as `r`/`n`
  reach the field instead of refresh/jump shortcuts. The enabled toggle uses
  `⌘E`/`Ctrl+E` (and a click) for the same reason.
- **The cron hint is conservative.** It recognizes scalar, step and wildcard
  time fields plus common day/weekday/month shapes; anything more complex still
  gets the computed next fire under `Custom schedule` rather than a wrong
  description.

### Step 10 deviations / additions

This step added no product features. It made the existing surfaces trustworthy
under keyboard-only use, failure, long lists and reduced-motion preferences.

#### Accessibility

- **Focus traversal is explicit.** The pinned GPUI build does not move focus on
  Tab by itself; the shell now binds `Tab`/`Shift+Tab` to `FocusNext`/`FocusPrev`
  and calls `window.focus_next()`/`focus_prev()` plus `cx.notify()` so the ring
  actually repaints. The palette, Catalog sheets, Schedules sheet and Settings
  keep their own, more specific Tab bindings (logical field focus), which take
  precedence by context depth.
- **Tab order is one scheme.** Top bar `1–2`, sidebar `10–15`, active surface
  `20–30`, detail column `50–67`, status bar `70–76`, overlays `100`. The old
  overlapping 20–35/40–54 range is gone, so Tab follows the visual order on wide
  windows and still follows it when the detail becomes a pushed page.
- **Every focusable control has a ring.** `components::with_focus` reserves a
  2 px border and paints it with the accent color on focus. The same helper now
  enforces the 32 px minimum target; primary actions use the 40 px token from
  `theme::Controls`. Compact density never shrinks either value.
- **Labels are visible, not icon-only.** Search-clear actions say **Clear**,
  the toast says **Dismiss**, and every error action says **Copy error**. Status
  remains icon + label + color everywhere (`✓ succeeded`, `✕ failed`,
  `⚠ awaiting input`, `◐ running`), never color alone. Declared choice lists and
  the theme/density chips mark the selected value with `✓`.
- **Focus returns to the shell** after a nav click, a `g` jump and a confirmed
  action, so Tab always starts from a predictable place. `Enter` on a focused
  button belongs to that button (the shell's list-open action checks the focused
  handle first).
- **Known framework limit.** `gpui 0.2.2` exposes no accessibility tree/AT-SPI
  API. The client therefore guarantees visible labels, focus order and keyboard
  operation, but a screen reader cannot consume widget roles or names from this
  build. This is recorded below as a limitation, not silently claimed as an
  ARIA implementation.

#### States audit

| Surface | Loading | Empty | Error / unauthorized |
|---|---|---|---|
| Inbox | static skeleton, previous rows stay | calm empty state + Catalog hint | full error panel with **Copy error** + Retry; refresh failures keep rows and show a copyable banner; unauthorized names the token path |
| Tasks | skeleton, previous rows stay | “No runs yet” vs “No matches” reset | full error panel / copyable banner; unauthorized names the token path |
| Task detail | per-tab skeleton | terminal vs running output, empty feed, no attempts | per-tab error state with **Copy error** + Retry; a failed refresh keeps the older feed and shows a banner |
| Catalog | skeleton | no definitions vs no search matches | full error panel / banner + preview error with **Copy error** + Retry |
| Workflows | root-list skeleton + inspect skeleton | no roots vs root without instances | root-list error and inspect error with **Copy error** + Retry; a stale snapshot keeps the older roots |
| Schedules | skeleton | empty trigger state | table error panel / banner + **Copy error** + Retry; sheet and delete rejections stay attached and copyable |
| Settings | agent roster/session-history loading line | explicit no-agents / no-persisted-sessions copy | connection pill + token path, reconnect/copyable connection notice, roster error with Retry, copyable test/save errors; past-session load failures stay inline and retryable |

No view renders a bare network error or a blank panel: each failure has a title,
the daemon/transport message, a next action and a copy control. Network and
persisted data are never `unwrap`/`expect`ed (the only `expect` is the initial
window creation, which is not network data).

#### Reduced motion

The shell reads the OS setting through `motion::prefers_reduced_motion()`:
`FAVETTO_REDUCE_MOTION` first, then `GTK_ENABLE_ANIMATIONS`, then
`gtk-enable-animations` in the GTK 3/GTK 4 `settings.ini`. The only decorative
motion is the 140 ms toast fade; reduced motion removes the fade and renders the
same final panel immediately. Skeleton rows are static, status changes are
text/icon changes, and no message is conveyed by motion alone.

#### Performance

- `ClientState` memoizes the Inbox grouping and the filtered Tasks list on a
  snapshot revision plus query/filter, and returns `Rc` handles. A render no
  longer re-groups or clones the 500-row snapshot.
- The Tasks table is a `uniform_list` (38 px rows); only the visible range is
  measured and painted, and keyboard selection reveals the row.
- The detail Events tab is GPUI's variable-height `list`, rebuilt only when the
  event revision changes. A paused reader keeps their logical position; a
  following reader stays at the tail.
- The live feed is capped at `MAX_RETAINED_EVENTS = 2_000`; the oldest frames
  are trimmed first, so the newest permission/error frames survive.

#### Keyboard-only pass (X11, live daemon)

Run on the local daemon with the pointer used only to give the window focus:

1. `g t` → Tasks; `↑`/`↓`, `Home`/`End`, `PageUp`/`PageDown` move the
   highlight; `↵` opened a run; Tab reached the detail tabs and header actions.
2. `g c` → Catalog; `↓` changed the definition; `↵` opened the launch sheet;
   `Tab` moved through the declared vars; `←`/`→` toggled the `supervise` /
   `escalate` choice; `Esc` closed the sheet without sending anything. `h`
   collapsed the highlighted folder and the selection moved to the next visible
   definition.
3. `g w` → Workflows; `]` changed the root (progress, buckets and DOT panel
   updated); `↑`/`↓` changed the instance; `c` opened the root-cancel
   confirmation and `Esc` backed out.
4. `g s` → Schedules; `n` opened the sheet; `Tab` moved focus to the task
   picker; `↓` moved the selection through the catalog; `⌘E` toggles enabled;
   `Esc` closed the sheet without writing.
5. `⌘,` → Settings; `Tab` moved the logical focus through Endpoint, Token,
   Test, Defaults, Save, Reconnect, Theme and Density; `⌘T` ran the connection
   test and showed latency + daemon cwd; `⌘↵` is the save chord.
6. `?` opened the shortcut sheet; `Esc` closed it; `⌘K` opened the palette and
   `Esc` returned to the list.

The live daemon had no blocked agent prompt at the time, so the permission
banner was verified through the existing `pending_request` unit tests and its
focusable 40 px reply buttons. Cancel/retry confirmations were opened and
dismissed without applying an RPC to a real run; the optimistic apply/rollback
path remains covered by the step-05 live RPC check and its unit tests.

#### Laws of UX design-review walk

| # | Question | Finding | Fix in this step |
|---|---|---|---|
| 1 | What decision is the user making, and how many options compete? | Inbox and Tasks still have one primary action; Catalog separates launch from one-shot. Selected filter/chips were color-only. | Added `✓` markers to selected filters, choices, theme and density; kept one primary per surface. |
| 2 | What must the user remember? | The detail header, palette recents and catalog collapse were already persistent. | Memoized derivations and kept the selection/scroll position across live refreshes. |
| 3 | Are related things grouped and unrelated separated? | Three regions and cards were sound; error actions were scattered as text. | Copy + Retry now form one action cluster on every failure surface; spacing follows `theme.rs`. |
| 4 | Is the primary action the most distinct thing? | Launch/Save were 40 px, but the Catalog “Start with variables” and permission buttons were smaller and quieter. | Raised both to the 40 px primary token; danger remains the only danger-colored action. |
| 5 | What is seen in the first 400 ms and while waiting? | Optimistic actions and skeletons existed; the toast was the only motion. | Kept static skeletons, added the reduced-motion switch and preserved optimistic reconciliation. |
| 6 | Does the flow match conventions? | `g` jumps and arrows did, but Tab did nothing because GPUI has no built-in traversal. | Added explicit `FocusNext`/`FocusPrev`, visible rings and shell focus after navigation. |
| 7 | What happens at the peak and the end, especially after error? | The outcome block existed, but a failure could end in a bare string. | Every failure now ends with Copy error + Retry; the transport keeps the older feed. |
| 8 | What input might be rejected unnecessarily? | Endpoint forms and typed values were liberal; choice-only vars required typing an exact string. | `←`/`→` cycles declared choices; the schedule task picker uses `↑`/`↓`. |
| 9 | Which 20% carries 80% of the value? | Inbox → Tasks → detail is the core loop; large snapshots were re-rendered. | Virtualized the task and event lists and cached the derivations. |
| 10 | Are we reducing complexity or moving it to the user? | SSE reconnect, resume cursors and rollback are absorbed by the client; Workflows exposes the raw complexity only as counts and buckets. | Kept that model; added copyable diagnostics rather than more controls. |

#### Known limitations

- **No accessibility tree in this GPUI build.** Widget roles/names are not
  exposed to AT-SPI; the guarantees are visual labels, focus order, keyboard
  operation and target sizes.
- **No text selection.** Output, source, DOT and errors are copyable via buttons
  (and error copy) instead of selectable text.
- **Restored transcripts are previews.** Settings caps the replayed text at
  4,000 characters; the TUI Agent panel is the full interactive transcript.
  A session whose daemon stopped before recording an ACP id remains `loading`
  and cannot be restored.
- **No IME marked-text composition.** Typed characters are appended at the end;
  cursor movement inside a field is not implemented.
- **No embedded Graphviz.** `workflow.get` is readable DOT source.
- **No daemon version.** `system.ping` does not report one; Settings says so.
- **No container queries.** The responsive breakpoint reads the window width.
- **The Workflows root and bucket lists are not virtualized.** They are derived
  from the same 500-row snapshot; the first 500 roots are still painted. The
  Tasks and Events lists, which are the long scroll surfaces, are virtualized.
- **The toast fade is the only animation.** If a future surface adds motion, it
  must call `motion::prefers_reduced_motion()` and preserve the final-state
  information.
