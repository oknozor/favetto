# Web client (Dioxus PWA)

Design and implementation plan for a browser/mobile client served by the daemon,
plus the HTTP + SSE offshoot of the wire protocol it needs.

Status: proposed
Scope: shared types (`crates/favetto-core`, new `favetto-wire` crate), daemon
(`crates/favetto`), new web client (`web/`), docs and CI.
Non-goal: replacing the TUI, adding an agent loop to the daemon, or full terminal
parity in v1. The embedded PTY/`vt100` pipeline stays the source of truth for
what the user sees and types; live terminal rendering is a later phase. All
judgment continues to live in the external supervisor (`docs/reference/supervisor-contract.md`).

---

## 1. Motivation

favetto is remote-TUI-first, but the *only* client is a Rust binary that must be
built and run on a workstation. The product's defining loop — long-running tasks
that periodically block on a human (`task_awaiting_input` → `agents.reply`) — is
exactly the loop an operator wants to service from a phone, a tablet, or a
browser, without a Rust toolchain.

Almost all of the substrate already exists and is client-agnostic:

- one wire protocol (`Frame` over MessagePack) with two transports;
- a token + short-lived pairing flow for onboarding;
- a durable, monotonic event log that doubles as a resume cursor;
- an `awaiting_input` detector and a structured reply channel;
- notification channels (`ntfy`) for out-of-band paging.

What is missing is a delivery surface. This document specifies a **Dioxus
(Rust→WASM) progressive web app** embedded in the daemon binary, fed by two new
HTTP endpoints: `POST /rpc` for commands and `GET /events` (Server-Sent Events)
for pushes. Because the client is Rust, it shares the wire types directly
instead of re-deriving them — which is what makes this approach cheaper and less
drift-prone than a TypeScript client.

---

## 2. Current state (verified against the tree)

| Claim | Evidence | Status |
|---|---|---|
| One MessagePack `Frame` protocol | `favetto-core/src/rpc/mod.rs` (`Frame`, `#[serde(tag = "type")]`) | present |
| Two transports, one dispatcher | `favetto/src/transport.rs` (`serve_unix`, `serve_socket`), `server.rs` (`serve_connection`) | present |
| Reusable request dispatcher | `server.rs` (`pub async fn dispatch(state, req) -> Response`) | present |
| WS auth via `Authorization` header only | `transport.rs` (`ws_handler`) | present |
| Event replay by cursor | `server.rs` (`EVENTS_SUBSCRIBE`, `db::events_after`), `db.rs` | present |
| Live push hub | `event_bus.rs` (`ServerPush::{Event, TaskUpdated, CatalogUpdated}`) | present |
| Pairing flow | `pair.rs` (`/pair/generate`, `/pair/exchange`, 60 s TTL) | present |
| Structured reply + input request | `favetto-core/src/agent_state.rs` (`InputReply`, `InputRequest`), `server.rs` (`agents.reply`) | present |
| Awaiting-input detection | `attention.rs` (screen poll + debounce) | present |
| Out-of-band notifications | `notify.rs` (`log`/`webhook`/`ntfy`), `notifications` table | present |
| HTTP static/SPA serving | — | **missing** |
| Browser client | — | **missing** |
| `POST /rpc` (HTTP request/response) | — | **missing** |
| `GET /events` (SSE) | — | **missing** |
| Browser-safe WS/SSE auth | — | **missing** |
| `favetto-core` compiles to `wasm32-unknown-unknown` | `config`, `dirs`, `toml`, `tokio-util` (codec), `std::fs` in `paths`/`tasks`/`workflow` | **missing** |

Two browser constraints drive the design and are worth stating up front:

1. The browser **WebSocket API cannot set an `Authorization` header**, so the
   existing `ws_handler` is unreachable from a web client as written.
2. The browser **`EventSource` API cannot set headers either**, and it reuses the
   same URL on auto-reconnect, so a single-use ticket in the query string breaks
   resumption. SSE must be read through a streaming `fetch` in the WASM client.

---

## 3. Design constraints

1. **The daemon never implements an agent loop.** The new endpoints are thin
   adapters over the existing `server::dispatch`; they add no policy, no
   scheduling, and no reasoning.
2. **`favetto-core` is I/O and platform aware; the new `favetto-wire` is not.**
   Wire types must build for `wasm32-unknown-unknown` with no filesystem, no
   `tokio`, no `dirs`.
3. **Wire changes are additive.** Any new field carries `#[serde(default)]` with
   a legacy-decode test, matching `model.rs` conventions.
4. **The generated reference must not drift.** `cargo run -p favetto -- __doc`
   must produce a clean `git diff` after any event/RPC/config change.
5. **One focused change at a time.** Conventional Commits, inline tests, and the
   full `AGENT.md` gate set before push.
6. **Single binary.** The web assets are embedded; the daemon gains no runtime
   asset directory it must be shipped with.
7. **The TUI is untouched.** The extraction in §5 is a refactor with re-exports,
   not a behavioural change, and every existing test stays green.

---

## 4. Scope decisions

**Adopt**

- `favetto-wire`, a wasm-clean crate of pure serde types, extracted from
  `favetto-core`.
- `POST /rpc`: one MessagePack `Frame::Request` in, one `Frame::Response` out,
  bearer-auth, delegating to `server::dispatch`.
- `GET /events`: an SSE stream with `id:` = the monotonic event id and
  `Last-Event-ID`/`last_event_id` resume, reusing `db::events_after`.
- `POST /auth/ticket`: short-lived, single-use tickets accepted as `?ticket=` by
  `ws_handler`, for the future terminal channel and any header-less client.
- A Dioxus client-only WASM app (`web/`), assets embedded via `rust-embed`, SPA
  fallback, `--web-dir` development override.
- SSE for the live in-app feed **and** the existing `ntfy` channel for background
  paging (they are complementary; see §9).
- A PWA shell: manifest, service worker, installability, `Notification` API.

**Adapt**

- **WebSocket stays, but not for v1 web commands.** WS remains the right channel
  for high-frequency bidirectional PTY I/O in the terminal phase and for the
  native TUI/MCP clients. The browser uses HTTP+SSE; the ticket exists so WS is
  reachable when the terminal lands.
- **Serving lives in the daemon**, not a separate `favetto web` binary: same
  origin removes CORS from the picture and keeps one process.
- **ntfy is the background channel.** Web Push (VAPID) is deferred until the
  third-party dependency is a real objection.

**Drop**

- JSON-Schema → TypeScript codegen and a Node build step: Dioxus shares the Rust
  types, so there is nothing to regenerate.
- Full terminal parity in v1 (deliberate; see §11 Phase 5).
- Any new `TaskStatus`, config-driven scheduling policy, or in-daemon supervisor.

---

## 5. Structural change: extract `favetto-wire`

### 5.1 Why

`favetto-core` is the natural home for shared types, but it is not
wasm-compatible: `config` + `dirs` + `toml` assume a native filesystem,
`tokio-util`'s codec pulls `tokio`, `paths.rs`/`tasks.rs`/`workflow.rs` call
`std::fs`, and `rpc/messages.rs` imports `crate::tasks::TaskVar`, so the RPC
types transitively depend on the catalog parser. `chrono`/`uuid` additionally
need wasm features (`wasmbind`, `js`) for `Utc::now()`/`Uuid::new_v4()`.

### 5.2 Module mapping

| Module | `favetto-wire` (pure) | `favetto-core` (I/O, native) |
|---|---|---|
| `rpc` envelope + `rpc::messages` | full | re-export |
| `model` (Task, Event, `into_notification`) | full | re-export |
| `agent_state` | full | re-export |
| `workflow` graph/inspect **types** | `WorkflowNode/Edge/Graph/Inspect/State`, cancel/create results | `build_graph`/`build_dot` builder + `needs` edge derivation |
| `task_var` (`TaskVar`) | full | re-export |
| `config`, `paths`, `auth` | — | full |
| `tasks` parser (`TaskDef`, `needs_parts`, `validate_needs`) | — | full |
| `wire` (length-prefixed framing, `FrameCodec`) | — | full |
| `ws` (frame ↔ WS payload) | — | full |

`favetto-core` preserves every existing import path by re-exporting:

```rust
// favetto-core/src/lib.rs
pub use favetto_wire::{agent_state, model};
pub use favetto_wire::rpc;            // or a thin module that `pub use`s it
pub use favetto_wire::workflow::*;    // types; the builder stays here
```

`favetto-core/src/workflow.rs` keeps `build_graph`/`build_dot` and begins with
`pub use favetto_wire::workflow::*;`; `favetto-core/src/tasks.rs` re-exports
`TaskVar`. The daemon, TUI, MCP, and doc generator keep compiling unchanged.

### 5.3 Dependencies and features

`favetto-wire` depends only on `serde`, `serde_json`, `rmp-serde`, `uuid`,
`chrono`, `thiserror`, and (behind a `schema` feature) `schemars`. Wasm builds
enable the platform features via target-specific declarations:

```toml
[features]
default = []
schema = ["schemars"]        # enabled by the daemon's docgen

[target.'cfg(target_arch = "wasm32")'.dependencies]
uuid = { version = "1", features = ["js"] }
chrono = { version = "0.4", features = ["wasmbind"] }
```

`favetto-core` enables `favetto-wire/schema` for the native docgen build; the web
client does not, keeping the WASM bundle lean.

### 5.4 Alternative considered

Adding a `wasm` feature to `favetto-core` that `#[cfg]`-gates the native modules
and marks `config`/`dirs`/`toml`/`tokio-util` optional. Rejected: cargo features
are additive and cannot subtract `std::fs` call sites cleanly, and the resulting
`cfg` lattice is harder to reason about than a crate boundary that mirrors the
project's existing one-concern-per-crate layout.

---

## 6. Transport: HTTP commands + SSE pushes

### 6.1 `POST /rpc` — commands

A single request/response round trip over HTTP, byte-compatible with the other
two transports.

| Aspect | Decision |
|---|---|
| Body | MessagePack `Frame::Request` (`application/x-msgpack`) |
| Response | MessagePack `Frame::Response` (`application/x-msgpack`) |
| Auth | `Authorization: Bearer <token>` (fetch can set it); HTTP 401 otherwise |
| Dispatch | `server::dispatch(&state, req)`; no logic duplicated |
| Limit | `wire::MAX_FRAME_LEN` (64 MiB) enforced by the axum body limit |
| Errors | Protocol errors travel inside the `Frame::Response`; malformed bodies are HTTP 400 |

Because the Dioxus client links `favetto-wire` and `rmp-serde`, it produces the
exact bytes the WS and Unix-socket paths do. This is the whole payoff of §5.

### 6.2 `GET /events` — SSE pushes

The push channel. It carries the same `Notification` values the WS path emits
(`ServerPush::into_notification`, `event_bus.rs`), encoded as JSON for
readability.

```
GET /events?last_event_id=4820
Authorization: Bearer <token>

HTTP/1.1 200 OK
Content-Type: text/event-stream
Cache-Control: no-cache
X-Accel-Buffering: no

id: 4821
event: event
data: {"id":4821,"kind":"task_finished","payload":{...},"created_at":"..."}

: keep-alive

id: 4822
event: task.updated
data: {"id":"...","name":"implement_issue","status":"running",...}
```

| Aspect | Decision |
|---|---|
| Replay | `db::events_after(&db, last_event_id, 500)` before switching to live delivery — the same helper `events.subscribe` uses (`server.rs:191`) |
| Cursor | `id:` carries the monotonic event id; the client persists the highest seen |
| Resume | `Last-Event-ID` header **or** `?last_event_id=` query (both accepted) |
| Non-event pushes | `task.updated` / `catalog.updated` carry no `id:`; only `event` frames advance the cursor |
| Heartbeat | `: keep-alive` every 15 s, to defeat idle proxy timeouts and surface dead clients |
| Backpressure | On broadcast `Lagged`, end the stream; the client reconnects with its persisted cursor and replays (mirrors `server.rs:80`) |
| Auth | Bearer header via a streaming-`fetch` reader in the WASM client, not `EventSource` |

The replay/live loop is extracted into one shared helper used by both the WS
`events.subscribe` handler and the SSE handler, so resume semantics cannot
diverge.

### 6.3 `POST /auth/ticket` — browser-safe WebSocket auth

```
POST /auth/ticket            (Authorization: Bearer <token>)
-> { "ticket": "…", "expires_in": 30 }
```

A ticket is short-lived (default 30 s) and single-use, stored in an in-memory
store shaped like `pair::PairStore`. `transport::ws_handler` accepts
`?ticket=<t>` in addition to the bearer header, consuming it via the same
constant-time verify path. v1 does not use WS from the browser; the endpoint is
built now so the terminal phase (§11 Phase 5) has a working authentication
mechanism, and so any header-less client has one.

### 6.4 Why not WebSocket for browser commands

It would work with the ticket from §6.3, but then the browser opens two
connections (WS for commands, SSE for pushes) and either duplicates push
handling or ignores the WS push stream. HTTP+SSE keeps one command path and one
push path, and the only place a browser cannot set headers — the WS upgrade — is
not on the v1 critical path.

---

## 7. Serving the SPA

| Route | Handler |
|---|---|
| `GET /` | embedded `index.html` |
| `GET /assets/*` | embedded hashed asset (`rust-embed`), immutable cache |
| non-API `GET` with `Accept: text/html` | SPA fallback → `index.html` |
| `/rpc`, `/events`, `/metrics`, `/pair/*`, `/webhooks/*`, `/agent-hooks/*` | never shadowed by the fallback |

- Assets are embedded with `rust-embed` into the `favetto` binary. A runtime
  `--web-dir` (and `[web].dir`) serves from disk for development.
- MIME types are explicit (`application/wasm`, `text/javascript`, `text/css`);
  `wasm` must not be served as `application/octet-stream`.
- `index.html` is `no-cache`; hashed assets are long-lived.
- If the binary was built without assets, `/` returns a short diagnostic page
  instead of a 500.
- The router is assembled in `daemon.rs` beside the existing `.route("/rpc", …)`
  and `.merge(…)` calls.

No COOP/COEP headers are required (Dioxus web uses no `SharedArrayBuffer`/
threads by default).

---

## 8. Dioxus application

### 8.1 Crate layout and workspace

- New top-level `web/` directory (parallel to `docs/`), excluded from the root
  cargo workspace (`[workspace] exclude = ["web"]`) so the native
  `cargo clippy --workspace`/`nextest` gates are unaffected. It carries its own
  `Cargo.toml` with a `[workspace]` table and a path dependency on
  `favetto-wire`.
- Client-only render mode (`dx build --platform web`); Dioxus fullstack/server
  functions are **not** used — the daemon is the server.
- Build output `web/dist/` is embedded by the `favetto` crate's build step. CI
  builds the web app before the binary and pins `dx` + `wasm-bindgen` to the
  Rust toolchain.

### 8.2 Client internals

- **Transport client**: `rmp-serde` encode/decode of `Frame` over
  `fetch("/rpc")` with the bearer header; a streaming-`fetch` SSE reader that
  sets `Authorization`, tracks `Last-Event-ID`, reconnects with backoff, and
  feeds a Dioxus signal.
- **State**: Dioxus signals; task/event/catalog caches keyed by id; the cursor in
  `localStorage`.
- **Routing**: Dioxus Router with deep-linkable routes so `ntfy` can link
  straight to work (`/inbox/:task_id`).

### 8.3 Views

| View | RPC/stream | Notes |
|---|---|---|
| **Inbox** (default on mobile) | `events` + `tasks.list` + `agents.list` | `awaiting_input` cards with `InputRequest` options; `agents.reply`; failures; recent notifications |
| **Tasks** | `tasks.list`, `tasks.get` | status filters, run history, output, `tasks.start`/`cancel`/`retry` |
| **Workflow** | `workflow.get`, `workflow.inspect`, `workflow.spawn/cancel/retry` | runtime buckets; DAG rendering deferred |
| **Catalog** | `catalog.list`, `catalog.get` | Markdown preview (`pulldown-cmark`) |
| **Schedules** | `schedules.list/upsert/delete` | cron management |
| **Notifications / Settings** | `notifications.list/test`, `hooks.upsert` | channel test, hook management, pairing/session info |

Markdown rendering uses a WASM-friendly Rust parser; the DAG view (SVG emitted
directly by Dioxus) is Phase 5.

### 8.4 Onboarding

The pair screen accepts the code from `favetto pair` (or `?pair=CODE`), calls the
existing `POST /pair/exchange`, and stores the returned token in `localStorage`.
The token is the same bearer token the TUI uses; rotating it on the daemon
(`favetto token-rotate`) invalidates the web session at its next connection.

---

## 9. Notifications

SSE and `ntfy` are complementary, not alternatives:

- **SSE** drives the live in-app experience — inbox badges, the browser
  `Notification` API while the PWA is open, and instant UI updates.
- **SSE cannot wake a closed or backgrounded app.** There is no live connection
  when the tab is not running.
- **`ntfy`** (already implemented in `notify.rs`, driven by `hooks.upsert` and
  the `attention.rs` watcher) pages the operator when the app is closed and
  deep-links back into `/inbox/:task_id`.

Configuration stays file-based for channels; the web Settings view manages
hooks (`hooks.upsert`) and can send a test (`notifications.test`).

---

## 10. Configuration

```toml
[web]
enabled = true        # serve the embedded SPA
dir = ""              # empty = embedded assets; a path = serve from disk (dev)
heartbeat_secs = 15   # SSE keep-alive interval

[auth]
ticket_ttl_secs = 30  # short-lived WS/SSE ticket lifetime
```

New fields are defaulted so existing configs decode unchanged; `__doc` is
regenerated in the same change.

---

## 11. Delivery phases

Each task is independently reviewable. Sizes: S ≈ half day, M ≈ 1–2 days,
L ≈ 3–5 days.

### Phase 0 — wire extraction (blocking)

- **P0.1** Create `favetto-wire`; move pure types; wasm feature plumbing; keep
  `favetto-core` re-exports and the TUI/daemon green · **M–L**
- **P0.2** `cargo check -p favetto-wire --target wasm32-unknown-unknown` in CI ·
  **S**
- **P0.3** Confirm `__doc` output is byte-identical after the move · **S**

### Phase 1 — HTTP + SSE

- **P1.1** `POST /rpc` over `server::dispatch` · **S–M**
- **P1.2** `GET /events` SSE with replay/heartbeat, sharing the subscribe helper ·
  **M**
- **P1.3** `POST /auth/ticket` + `?ticket=` in `ws_handler` · **S**
- **P1.4** `[web]`/`[auth]` config + `__doc` · **S**

### Phase 2 — serving

- **P2.1** `rust-embed` static handler, MIME, SPA fallback, `--web-dir` · **M**
- **P2.2** `web/` scaffold, `dx build`, embed in the binary, CI job · **M–L**

### Phase 3 — client

- **P3.1** Transport client (msgpack + SSE) and connection state · **M**
- **P3.2** Read-only: Inbox, Tasks, Catalog, live event feed · **L**
- **P3.3** Actions: `agents.reply`, start/cancel/retry, `workflow.*` · **M–L**
- **P3.4** Schedules, Notifications, Settings, pairing screen · **M**

### Phase 4 — PWA polish

- **P4.1** Manifest + service worker, installability, offline shell · **M**
- **P4.2** `Notification` API + ntfy deep links; mobile layout/a11y · **M**

### Phase 5 — optional

- **P5.1** SVG workflow DAG · **M**
- **P5.2** Live terminal over the ticket-authenticated WS (`agents.attach`,
  `agents.input`, `agents.resize`) + xterm-style renderer · **L**

### Dependency graph

```text
P0.1 ─► P0.2
  └───► P0.3
P0.1 ─► P1.1 ─┐
P0.1 ─► P1.2 ─┼─► P3.1 ─► P3.2 ─► P3.3 ─► P3.4 ─► P4.1 ─► P4.2
P0.1 ─► P1.3 ─┘
        P1.4 ─┘
P2.1 ◄─ P1.1/P1.2        P2.1 ─► P2.2 ─┘
P1.3 ─► P5.2
P3.2 ─► P5.1
```

**Smallest valuable milestone** (monitor + act from a phone): `P0.*, P1.*, P2.*,
P3.1–P3.3`.

Suggested commit split: `refactor(core): extract favetto-wire`, `feat(api): add
POST /rpc over HTTP`, `feat(api): add SSE event stream`, `feat(auth): add
short-lived websocket tickets`, `feat(web): serve embedded SPA`, `feat(web):
add Dioxus client shell`, then one `feat(web): …` per view. Use `feat!` only if a
field becomes required.

---

## 12. Testing

- **Wasm build**: `cargo check -p favetto-wire --target wasm32-unknown-unknown`
  (P0.2); a host build of `web/` via `dx build`.
- **`POST /rpc`**: integration round trip for a representative request
  (`tasks.list`) and an error (`INVALID_PARAMS`); body-limit rejection.
- **SSE**: subscribe, `tasks.start`, assert a frame arrives; `Last-Event-ID`
  replay returns exactly the missed events; heartbeat emitted; `Lagged` ends the
  stream.
- **Ticket**: issue → use once → reuse rejected → expiry rejected.
- **Static**: `/` returns 200 HTML; asset MIME is correct; SPA fallback serves
  `index.html`; API routes are never shadowed.
- **Compat**: existing `favetto-core`/daemon/TUI tests unchanged and green;
  legacy `Frame`/`Task` payloads decode.
- **Docs**: `cargo run -p favetto -- __doc` diff clean and `docs:check` passes.
- **Gates** from `AGENT.md` (`fmt`, `clippy -D warnings`, `nextest`, `deny`) plus
  the new wasm job.

---

## 13. Risks and mitigations

| Risk | Mitigation |
|---|---|
| The `favetto-wire` extraction perturbs the daemon/TUI/MCP | Do it first, in one focused change, with re-exports and every existing test green before any endpoint lands |
| `dx`/`wasm-bindgen` versions drift from the Rust toolchain | Pin both in CI and document the pairing next to the existing toolchain note |
| SSE arrives in clumps behind a reverse proxy | `X-Accel-Buffering: no` plus documented nginx/Caddy buffering settings; the heartbeat surfaces a wedged stream |
| SSE auth cannot use `EventSource` | A streaming-`fetch` reader in the WASM client sets the bearer header and owns reconnect/`Last-Event-ID` |
| EventSource-style single-use tickets break SSE reconnect | Tickets are for WS (terminal) only; SSE uses the bearer header |
| `/pair/*` is unauthenticated and grants the token | Already true today; document the VPN/TLS requirement and consider requiring loopback or an existing token for `/pair/generate` |
| Static fallback shadows an API route | Explicit prefix exclusion in §7 and a test asserting API routes are unaffected |
| Unbounded asset growth / binary bloat | Hashed assets, gzip/br if the proxy provides it; the WASM bundle is measured and budgeted in CI |
| Scope creep toward terminal parity | Phase 5 is explicitly optional and separately sized |

---

## 14. Resolved decisions

1. **Dioxus (Rust→WASM), client-only, single binary.** No Node toolchain and no
   TypeScript codegen; the client links `favetto-wire` and produces byte-identical
   frames. Assets are embedded with `rust-embed`; `--web-dir` is a dev override.
2. **Commands use `POST /rpc`; pushes use SSE.** HTTP+SSE avoids the browser
   WebSocket header limitation on the v1 path and reuses `server::dispatch`. WS
   remains for the terminal phase.
3. **Short-lived, single-use WS tickets** (`POST /auth/ticket`) authenticate the
   future terminal channel and any header-less client.
4. **SSE for the live in-app feed; `ntfy` for background paging.** SSE cannot wake
   a closed app, and `ntfy` already exists with zero new daemon code.
5. **v1 is triage + monitor.** Live terminal and the visual DAG are deferred to
   Phase 5.
