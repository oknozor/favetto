agent = "opencode"
provider = "opencode-go"
model = "deepseek-v4.1-flash"
worktree = false
permission = "allow"
---

You are implementing **step 01 of 11** of the favetto GPUI desktop client.

Read first:
- `docs/design/gpui-client.md` — the UX plan, screens and architecture you must follow.
- The existing client for API usage only (do not modify it): `web/src/client.rs`,
  `web/src/sse.rs`, `crates/favetto-wire/src/rpc/`.

## Goal

Create the `gpui/` desktop client skeleton as a **standalone cargo workspace**
(the same pattern as `web/`), with a window, design tokens and an empty shell.

## Work

1. Create `gpui/`:
   - `gpui/Cargo.toml`: `[workspace]`, package `favetto-gpui`, edition 2021,
     `rust-version = "1.94"`, license `MIT OR Apache-2.0`, `publish = false`.
   - Dependencies: `gpui = "=0.2.2"` (pin it; do **not** use the Zed git
     dependency), `favetto-wire = { path = "../crates/favetto-wire" }`,
     `serde`, `serde_json`, `rmp-serde`, `uuid`, `chrono`, `anyhow`,
     `thiserror`, `reqwest` (rustls-tls + stream), `tokio` (rt-multi-thread,
     macros, sync, time), `futures-util`, `async-channel`, `dirs`, `toml`,
     `base64`, `fuzzy-matcher`.
   - **Required workaround**: add `libc = "=0.2.188"` with this comment:
     `gpui 0.2.2 -> gpui_http_client -> zed-async-tar -> xattr 0.2.3` uses
     `libc::ENOATTR`, which libc removed for Linux in 0.2.189. The pin is the
     smallest fix; keep it until gpui updates `xattr`. Verify
     `cargo check` succeeds; if it does not, investigate before proceeding.
   - `gpui` 0.2.2 is published with `Application::new()` (the
     `gpui_platform` crate is **not** on crates.io). Follow
     `gpui/examples/hello_world.rs` from the downloaded crate for the exact
     imports.
   - `gpui/.gitignore` with `target/`.
2. `gpui/src/main.rs`: `gpui::Application::new().run(...)`, open a window titled
   "favetto" (default 1280×800, minimum 900×600), install the root view.
3. `gpui/src/theme.rs`: the token set from the design doc (spacing, radii, type
   scale, semantic colors, dark/light following the OS appearance).
4. `gpui/src/app.rs`: a root view with the three-region shell (sidebar, content,
   status bar) and placeholder routes for Inbox, Tasks, Catalog, Workflows,
   Schedules, Settings. No data yet.
5. `gpui/README.md`: what it is, prerequisites (`libxkbcommon`, Wayland/X11,
   fontconfig on Linux), `cargo run`, and how to point it at a daemon.
6. Add `"gpui"` to the root `Cargo.toml` `exclude` list so the daemon's native
   gates stay untouched.

## Constraints

- Work only in `gpui/`, plus the one root `Cargo.toml` exclude edit.
- Do not modify `crates/`, `web/`, or the daemon.

## Definition of done

- `cd gpui && cargo check` succeeds (the first GPUI build is long; allow it).
- `cargo fmt --check` and `cargo clippy -- -D warnings` succeed in `gpui/`.
- The app opens a window with the shell and six navigation entries.
- `cargo test --workspace` at the repository root still passes (GPUI is excluded).

## Commit

- After the definition of done is satisfied, create a git commit for this task before reporting completion.
- Review `git status` and the diff; stage only changes made for this task. Do not include pre-existing or unrelated changes, and never amend an existing commit.
- If task changes overlap pre-existing edits and cannot be cleanly isolated, stop and ask before committing.
- Use a concise commit message, e.g. `feat(gpui): scaffold desktop client`.
