# favetto GPUI desktop client

Status: **scaffold (step 01)** — a standalone cargo workspace with a window,
the design tokens, and the empty three-region shell (sidebar / content /
status bar) with the six navigation entries. The daemon transport
(MessagePack RPC + authenticated SSE) lands in step 02 (`src/transport.rs`,
`src/sse.rs`); the plans live in `docs/design/gpui-client.md`.

## What it is

A native desktop client for a running favetto daemon. GPUI is the GPU
accelerated Rust UI framework the Zed editor is built on. The client mirrors
the daemon's vocabulary (`Inbox → Tasks → Catalog → Workflows → Schedules →
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

The window opens titled **"favetto"** (default 1280×800, minimum 900×600).

## Pointing it at a daemon

Step 02 wires the connection: the token from `~/.local/share/favetto/token`
and the base URL from `~/.config/favetto/gpui.toml` (persisted there; overridable
in Settings). This scaffold only opens the shell — no RPC is issued yet.

## Development

```sh
cargo fmt --check
cargo clippy -- -D warnings
cargo test --workspace   # from the repo root (gpui is excluded there)
```

Deviations from the plan are recorded in this file as the chain progresses
(docs/design/gpui-client.md §5, cross-cutting constraint 4).
