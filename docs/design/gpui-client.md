# GPUI desktop client — UX plan and implementation chain

Status: proposed (implemented through the `gpui/*` task chain)
Scope: a **native desktop client** for a running favetto daemon.
Non-goals: replacing the Dioxus web/Android client, embedding an editor, or
re-implementing the agent runtime.

GPUI is a GPU-accelerated Rust UI framework (the one behind Zed). This client is
a second presentation layer over the existing daemon API: the MessagePack RPC
for commands and the authenticated SSE stream for pushes.

---

## 1. Product shape

The daemon already defines the vocabulary. The GPUI client mirrors it rather
than inventing a new one, so users can move between the TUI, the web client and
the desktop app without relearning the product.

```
┌────────────────────────────────────────────────────────────────────┐
│  favetto          repo: favetto        ● connected     ⌘K  ⚙       │
├──────────┬───────────────────────────────────────┬─────────────────┤
│ Inbox  ! │  Needs you (2)                        │  Task detail     │
│ Tasks    │  ┌─────────────────────────────────┐  │  name / status   │
│ Catalog  │  │ opencode · repo:favetto/plan    │  │  agent  attempt  │
│ Workflows│  │ awaiting permission  ▶          │  │  ── tabs ──      │
│ Schedules│  └─────────────────────────────────┘  │  Output Events   │
│ Settings │  Recent                               │  Runs            │
│          │  …                                    │  [Cancel] [Retry]│
└──────────┴───────────────────────────────────────┴─────────────────┘
```

- **Navigation order** (Inbox → Tasks → Catalog → Workflows → Schedules →
  Settings) follows the **Serial Position Effect**: the two surfaces users need
  most (what needs me, what ran) are first; configuration is last.
- **Three regions** — navigation, working list, detail — use the **Law of
  Common Region** and **Law of Proximity** to keep related controls inside one
  visual boundary. The detail column belongs to the run lists (Inbox and Tasks);
  Catalog, Workflows, Schedules and Settings use the full width. On narrow
  windows the detail becomes a pushed page instead of a third column (GPUI
  `container_query`); on wide windows the detail/list boundary is a resizable
  divider, session-scoped and clamped.
- **One primary action per view** (**Hick's Law**, **Choice Overload**). Inbox
  rows: *open*. Task detail: *answer permission* when blocked, otherwise
  *cancel/retry*. Everything else lives behind the command palette or a menu.
- **Keyboard first** (**Fitts's Law**): every list is operable with ↑/↓/Enter;
  `⌘K` (Ctrl+K) opens a searchable command palette; `g`-prefixed jumps
  (`g i`, `g t`, `g c`, `g w`) mirror the TUI's muscle memory (**Jakob's Law**).

### Surfaces

| Surface | Job | Primary action |
|---|---|---|
| **Inbox** | "What needs me now?" | Open the blocked/failed run |
| **Tasks** | All runs, filter and search | Open detail; cancel/retry |
| **Task detail** | Understand and steer one run | Answer permission, cancel, retry |
| **Catalog** | Find and launch a definition | Start with variables |
| **Workflows** | One root's DAG and progress | Inspect; cancel root / retry node |
| **Schedules** | Recurring triggers | Create/edit/delete |
| **Settings** | Connection, agents, theme | Test connection |

---

## 2. Law-backed design decisions

Each row names the element, the change, and the expected effect.

| Law | Decision | Tradeoff |
|---|---|---|
| **Jakob's Law** | Same IA, terminology and status badges as the TUI/web (`awaiting input`, `repo:<name>/<task>`, `attempt`). | Less freedom for novel layouts; no learning cost. |
| **Hick's Law** | One primary button per screen; secondary actions in a `⋯` menu or the palette. | Advanced actions are one step deeper. |
| **Choice Overload** | Catalog groups by repo and filters by text; the launch form only shows declared `[[vars]]`. | Users can't launch with ad-hoc JSON (intentional). |
| **Fitts's Law** | Rows ≥ 32 px high, primary buttons ≥ 40 px, destructive actions separated by ≥ 8 px; keyboard equivalents for every command. | Slightly airier lists. |
| **Doherty Threshold** | Every RPC gets an immediate local state change (optimistic cancel/retry) and a spinner within 100 ms; the connection banner appears after a single failed heartbeat. | Optimistic state must be reconciled on error. |
| **Peak-End Rule** | Task detail ends with a clear outcome block (succeeded/failed/cancelled + duration + usage); errors carry a copyable message and a retry action. | More vertical space in the detail pane. |
| **Law of Common Region** | Sidebar, list and detail are three bounded surfaces with 1 px separators and consistent 12/16 px insets. | Less "edge-to-edge" density. |
| **Law of Proximity** | Status, agent and repo form one metadata cluster; actions are separated by a spacer. | Requires consistent spacing tokens. |
| **Miller's Law / Chunking** | Inbox groups "Needs you" vs "Recent"; task detail uses three tabs (Output, Events, Runs); settings split into Connection / Agents / Appearance. | Some content is one click away. |
| **Von Restorff** | Awaiting-input rows use an accent stripe + icon; destructive buttons are the only danger-colored control on screen. | Accent must remain accessible in both themes. |
| **Goal-Gradient** | Workflows show `done / total` and a progress bar; the task header shows `attempt n of m` when retries are configured. | Progress must not be faked when totals are unknown. |
| **Working Memory** | The detail header stays pinned with name, status, agent, repo and elapsed time; the palette remembers the last query per session. | Slightly taller header. |
| **Postel's Law** | Connection settings accept `host:port`, `http(s)://…`, or a full `/rpc` URL; tokens are trimmed; errors say what to fix. | More normalization code. |
| **Tesler's Law** | The client absorbs SSE reconnection, event ordering and resume cursors; the user only sees "reconnecting…" and a retry count. | Hidden complexity; must be tested. |
| **Aesthetic-Usability** | A small, consistent token set (spacing, radii, type scale, two themes) applied everywhere. | Visual polish must not substitute for state coverage. |

Accessibility is a constraint, not a tradeoff: focus rings are always visible,
status is never color-only (icon + label), all interactive elements are
keyboard-reachable, and motion respects the OS "reduce motion" setting.

---

## 3. Visual system

- **Tokens** in one module: spacing 4/8/12/16/24, radii 4/8/12, type scale
  11/13/15/20/28, two themes (dark/light) flipped by the OS appearance.
- **Semantic colors**: `bg`, `surface`, `border`, `text`, `muted`, `accent`
  (running/open), `warning` (awaiting input), `danger` (failed), `success`.
- **Status model**: icon + label + color, never color alone. `pending`,
  `running`, `awaiting input`, `succeeded`, `failed`, `cancelled`.
- **Density**: comfortable by default (32 px rows), compact at < 900 px height.

---

## 4. Architecture

`gpui/` is a **standalone cargo workspace** (like `web/`), excluded from the
root workspace so the daemon's native `clippy`/`nextest` gates stay fast.

```
gpui/
  Cargo.toml            # [workspace] + crate deps (gpui, favetto-wire, reqwest…)
  src/main.rs           # Application::new().run(open window)
  src/app.rs            # root view, routing, command palette, global state
  src/transport.rs      # MessagePack RPC over POST /rpc
  src/sse.rs            # incremental SSE parser + reconnect/backoff
  src/runtime.rs        # tokio thread ⇄ GPUI channel bridge
  src/state.rs          # ClientState entity: tasks, sessions, catalog, events
  src/theme.rs          # tokens + light/dark
  src/views/{inbox,tasks,task_detail,catalog,workflows,schedules,settings}.rs
  src/components/{…}    # rows, badges, buttons, banners, empty states
```

- **Transport**: reuse the `favetto-wire` types. `POST /rpc` carries one
  MessagePack `Frame::Request`; the response is one `Frame::Response`. `GET
  /events` is SSE with `Last-Event-ID` resume (the same protocol the web client
  uses).
- **Concurrency**: a dedicated `tokio` runtime on a background thread owns
  `reqwest`. The UI talks to it through `async-channel`; a GPUI background task
  drains updates and calls `cx.notify()` on the foreground.
- **State**: one `ClientState` entity holds the connection, the task list, the
  session map, the catalog and the feed cursors. Views are entities that read
  it; no global mutable statics.
- **Auth**: token from `~/.local/share/favetto/token` by default, overridable in
  Settings (persisted in `~/.config/favetto/gpui.toml`).
- **Tests**: pure units for framing, SSE parsing, URL normalization and status
  derivation; GPUI `TestAppContext` for the palette and list navigation.

---

## 5. Delivery chain

Each step is one favetto catalog task under `.favetto/tasks/gpui/`, chained with
`needs = "<previous>:succeeded"`, `worktree = false` (changes accumulate in the
same checkout) and `permission = "allow"` (unattended edits). Firing
`gpui/01-scaffold` starts the whole chain.

| Task | Deliverable | Definition of done |
|---|---|---|
| `01-scaffold` | Standalone workspace, window, tokens, empty shell, README | `cargo check` in `gpui/`; root workspace unchanged |
| `02-transport` | RPC + SSE + config/token + connection state | unit tests for framing/SSE; connects to a live daemon |
| `03-shell` | Sidebar, status bar, palette skeleton, shortcuts | every route reachable by keyboard |
| `04-inbox` | Grouped inbox, live updates, open task | blocked runs appear within one SSE tick |
| `05-tasks` | List, search, filters, cancel/retry | optimistic actions reconcile on error |
| `06-task-detail` | Header, tabs, event feed, permission replies | a blocked run can be answered without leaving the app |
| `07-catalog` | Browse/search/preview, vars form, launch | launching a catalog task succeeds end to end |
| `08-workflows` | List + inspect, progress, bulk actions | root cancel and node retry work |
| `09-schedules-settings` | Schedules CRUD + settings | connection test + agent list work |
| `10-polish` | A11y, empty/error/loading, reduced motion, performance | keyboard-only pass; no known UX gaps |
| `11-verify` | Full checks, live smoke run, verification note | `cargo fmt/clippy/test` clean; documented results |

### Cross-cutting constraints for every step

1. Use the daemon API only; never read SQLite or the filesystem for product
   data (the token/config files are the exception).
2. Keep the existing TUI, web and daemon crates untouched except for the
   documented `gpui/` additions and the root `exclude` list.
3. Run `cargo fmt --check`, `cargo clippy -- -D warnings` and `cargo test` in
   `gpui/` before declaring a step done.
4. Record any deviation from this plan in `gpui/README.md`.

---

## 6. Risks

- **GPUI churn**: pin the `gpui` version and isolate framework-specific code in
  `src/views`; upgrading is a contained change.
- **First build cost**: GPUI compiles a large dependency graph once; CI should
  cache the `gpui/` target directory.
- **SSE ordering**: the resume cursor must never move backwards; test with
  duplicated and out-of-order events.
- **Permission semantics**: permission requests are session-scoped; the detail
  view must correlate `request_id` exactly and never guess.
- **Scope**: this is a desktop client, not a terminal emulator. Agent sessions
  are rendered as structured events, not ANSI frames.
