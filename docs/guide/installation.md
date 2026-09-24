# Installation

favetto is a single Rust binary. You need a Rust toolchain and at least one
external coding-agent CLI for it to orchestrate.

## Prerequisites

- **Rust** 1.92 or newer (edition 2021): <https://rustup.rs>.
- **An agent CLI** on your `PATH` — for example
  [opencode](https://opencode.ai), Claude Code, pi, or Mistral Vibe. favetto
  never implements its own agent loop, so a task cannot start without one.

## Build from source

```bash
git clone https://github.com/oknozor/favetto
cd favetto
cargo build --release
```

The binaries land at `target/release/favetto` (daemon) and
`target/release/favetto-tui` (TUI client). To put both on your `PATH`:

```bash
cargo install --path crates/favetto
cargo install --path crates/favetto-tui
```

::: tip One command to attach
The `favetto` launcher execs the sibling `favetto-tui` binary for
`favetto tui …`, so install both to keep that invocation working.
:::

## Verify

```bash
favetto --version
```

```text
favetto 0.1.0
```

::: tip No configuration required to start
The four built-in agents (opencode, claude, pi, vibe) are always registered, so
an empty config exposes them. Add a `[agents.*]` section only when you want to
override an invocation — see [Embedded agents](./agents).
:::

Next: [Getting started](./getting-started).
