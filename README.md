# favetto

A long-running, LLM-driven agent orchestrator with a **remote-TUI-first** design.
It schedules and runs declarative tasks by delegating to external coding-agent CLIs
(opencode, Claude Code, pi, Mistral Vibe, …), embedded as live terminals — the
orchestrator never implements its own agent loop.

```
┌───────────────────────────────────────────────────────────────┐
│                  TUI Client (ratatui)                         │
│   local attach  ─► unix:///tmp/favetto.sock                   │
│   remote attach ─► ws://host:7878/rpc?token=…                 │
│   (same MessagePack wire protocol over both)                  │
└───────────────────────────────────────────────────────────────┘
                             │
┌───────────────────────────────────────────────────────────────┐
│                  Favetto Daemon                               │
│   Scheduler ─► Task Queue ─► Agent Sessions (PTY) ─► Agent CLIs│
│   Event Bus · Hook Engine · Notification Bus                  │
│   Persistence (SQLite/sqlx) · Remote API (axum)               │
└───────────────────────────────────────────────────────────────┘
```

> **Documentation:** <https://oknozor.github.io/favetto/> — the full manual:
> a task-oriented guide and a complete CLI/config/task/event/API reference.

## Install

```bash
git clone https://github.com/oknozor/favetto
cd favetto
cargo build --release
cargo install --path crates/favetto
```

You also need an agent CLI (for example opencode) on your `PATH`. See the
[installation guide](https://oknozor.github.io/favetto/guide/installation).

## Quickstart

```bash
# terminal 1 — the daemon
./target/debug/favetto daemon

# terminal 2 — the TUI (local attach over the unix socket)
./target/debug/favetto tui

# or attach remotely over WebSocket (token auto-created in ~/.local/share/favetto/token)
./target/debug/favetto tui --remote ws://127.0.0.1:7878/rpc \
    --token-file ~/.local/share/favetto/token
```

Write a task in `tasks/hello.md` (a TOML header, then a Markdown prompt):

```md
agent = "opencode"
---
Create a file named hello.txt containing the single line `hello from favetto`.
```

Then start it from the **Catalog** tab. The
[first-task walkthrough](https://oknozor.github.io/favetto/guide/first-task)
covers this end to end.

## Documentation

The published site is the manual:

| Page | Covers |
|------|--------|
| [Guide](https://oknozor.github.io/favetto/guide/installation) | Install, first task, catalog, variables, agents, schedules, webhooks, worktrees, signing, remote access, TUI. |
| [CLI reference](https://oknozor.github.io/favetto/reference/cli) | Every subcommand and flag, generated from clap. |
| [Configuration reference](https://oknozor.github.io/favetto/reference/config) | Every config section and field, generated from the Rust types. |
| [Task file format](https://oknozor.github.io/favetto/reference/tasks) | Header keys, `[[vars]]`, and prompt templates. |
| [Event kinds](https://oknozor.github.io/favetto/reference/events) | Every event on the bus, generated from the enum. |
| [Remote API](https://oknozor.github.io/favetto/reference/remote-api) | Methods, pushes, and error codes, generated from the RPC constants. |
| [Architecture](https://oknozor.github.io/favetto/reference/architecture) | Design and milestones. |

The [annotated `config.example.toml`](config.example.toml) is the canonical
starting configuration.

## Workspace layout

```
crates/
  favetto-core/          shared wire protocol + domain types (no HTTP/DB/LLM)
  favetto-providers/     provider/model catalog (opencode auth + models.dev)
  favetto-tui/           dependency-light terminal client binary
  favetto/               daemon binary + doc generator
tasks/                   task catalog: `*.md` files with TOML header + prompt
docs/                    VitePress site (guide + generated reference)
```

## Checks

CI runs the same gates locally:

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo nextest run --locked --workspace   # install: cargo install cargo-nextest

# regenerate the reference and fail if it drifts
cargo run -p favetto -- __doc
git diff --exit-code -- docs/reference docs/public/favetto-schema.json

# build the site (fails on dead internal links)
cd docs && npm ci && npm run docs:check
```

### Fuzzing

The wire decoder ships a [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) target
under `fuzz/`. It needs a nightly toolchain and is not part of CI:

```bash
cargo install cargo-fuzz
cd fuzz
cargo +nightly fuzz run decode
```

The stable property tests above run in CI instead.

## Releasing

Releases are cut from `main` with [cocogitto](https://github.com/cocogitto/cocogitto)
(Conventional Commits + SemVer). Run the **Release** GitHub Actions workflow and
choose a bump (`auto` is the default); it runs the CI gates, bumps
`[workspace.package] version`, updates `CHANGELOG.md`, tags `vX.Y.Z`, and opens a
GitHub release. See the [releasing guide](https://oknozor.github.io/favetto/guide/releasing)
or `docs/guide/releasing.md`.
