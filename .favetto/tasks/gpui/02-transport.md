agent = "opencode"
provider = "opencode-go"
model = "deepseek-v4.1-flash"
worktree = false
permission = "allow"
needs = "gpui/01-scaffold:succeeded"
---

You are implementing **step 02 of 11** of the favetto GPUI desktop client.

Read first:
- `docs/design/gpui-client.md` (§4 Architecture, §2 decisions).
- `gpui/README.md` (progress from step 01).
- `web/src/client.rs` and `web/src/sse.rs` — reference implementations to port
  (do not depend on the `web` crate).
- `crates/favetto-wire/src/rpc/` — the typed API.

## Goal

Give the app a real transport: authenticated MessagePack RPC over `POST /rpc`,
SSE over `GET /events` with resume, and a connection state the UI can render.

## Work

1. `gpui/src/config.rs`: load `~/.config/favetto/gpui.toml`
   (`endpoint`, `token`), falling back to `http://127.0.0.1:7878` and the token
   file `~/.local/share/favetto/token`. Normalize `host:port`, `http(s)://…`
   and full `/rpc` URLs (Postel's Law).
2. `gpui/src/transport.rs`: typed RPC client over `favetto-wire` frames.
   - One `Frame::Request` in, one `Frame::Response` out.
   - Bearer auth; 401 → `Unauthorized`, malformed → protocol errors.
   - Typed wrappers for `system.ping`, `tasks.list`, `tasks.get`,
     `tasks.runs`, `tasks.events`, `tasks.cancel`, `tasks.retry`,
     `tasks.start`, `catalog.list`, `catalog.get`, `agents.list`,
     `agents.reply`, `workflow.inspect`, `workflow.cancel`, `workflow.retry`,
     `schedules.*`, `events.subscribe`.
3. `gpui/src/sse.rs`: incremental SSE parser (id/event/data/retry, CRLF,
   multi-line data, UTF-8 split across chunks), monotonic cursor, exponential
   reconnection, `Last-Event-ID` resume, 401 stop.
4. `gpui/src/runtime.rs`: a dedicated tokio runtime thread bridged to GPUI with
   `async-channel`. Commands go in; connection state and decoded pushes come
   out. The foreground drains updates and calls `cx.notify()`.
5. `gpui/src/state.rs`: initial `ClientState` entity: connection state, config,
   and the last `system.ping` result. Wire a temporary status line and
   "Reconnect" action into the shell.
6. Unit tests for: frame encode/decode, URL normalization, SSE parser, backoff,
   cursor monotonicity, and 401 handling.

## Constraints

- Work only in `gpui/`. Do not modify the daemon, TUI or web client.
- No unwrap/expect on network data; errors are values shown in the UI.

## Definition of done

- `cd gpui && cargo check && cargo test && cargo fmt --check && cargo clippy -- -D warnings`.
- With the daemon running, the app shows `Connected` and the ping round-trip
  result; stopping the daemon flips to `Reconnecting`, restarting recovers, and
  a bad token shows `Unauthorized` with the token field path.

## Commit

- After the definition of done is satisfied, create a git commit for this task before reporting completion.
- Review `git status` and the diff; stage only changes made for this task. Do not include pre-existing or unrelated changes, and never amend an existing commit.
- If task changes overlap pre-existing edits and cannot be cleanly isolated, stop and ask before committing.
- Use a concise commit message, e.g. `feat(gpui): add daemon transport`.
