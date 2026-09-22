---
layout: home

hero:
  name: favetto
  text: Remote-TUI-first agent orchestration
  tagline: A long-running, LLM-driven agent orchestrator that schedules and runs declarative tasks by delegating to external coding-agent CLIs — the orchestrator never implements its own agent loop.
  image:
    src: /logo.svg
    alt: favetto
  actions:
    - theme: brand
      text: Get started
      link: /guide/installation
    - theme: alt
      text: Your first task
      link: /guide/first-task
    - theme: alt
      text: GitHub
      link: https://github.com/oknozor/favetto

features:
  - title: Agents as workers
    details: favetto orchestrates; the coding loop is an external agent CLI embedded as a live PTY session that can be attached, reattached, and keystroke-driven from the TUI.
  - title: Remote-TUI-first
    details: The TUI client and the daemon are decoupled over a single wire protocol; local attach is just a special case of remote attach.
  - title: Declarative tasks
    details: Each task is a Markdown file with a TOML header and a prompt body, chainable with schedules, dependencies, and spawned children.
  - title: Crash-resilient
    details: Tasks and events persist across restarts; the append-only event log doubles as the TUI's resume cursor.
---

![favetto TUI — Catalog tab](/screenshots/tui-catalog.svg)

*The Catalog tab: folder tree + preview pane.*

## Installation

```bash
git clone https://github.com/oknozor/favetto
cd favetto
cargo build --release
cargo install --path crates/favetto
```

You also need an agent CLI (for example opencode) on your `PATH`. See
[Installation](/guide/installation) for prerequisites.

## Quickstart

```bash
# terminal 1 — the daemon
favetto daemon

# terminal 2 — the TUI (local attach over the unix socket)
favetto tui

# or attach remotely over WebSocket (token auto-created in ~/.local/share/favetto/token)
favetto tui --remote ws://127.0.0.1:7878 \
    --token-file ~/.local/share/favetto/token

# then write tasks/hello.md and start it from the Catalog tab
```

New here? Follow [Your first task](/guide/first-task) — it creates a task, runs
it through an agent, and shows the resulting events, with a screenshot at each
step.

## Where to go next

- **Guide** — [Installation](/guide/installation),
  [Your first task](/guide/first-task), [Catalog](/guide/catalog),
  [Variables & prompts](/guide/variables-and-prompts),
  [Embedded agents](/guide/agents),
  [Schedules & dependencies](/guide/schedules-and-dependencies),
  [Webhooks](/guide/webhooks), [TUI](/guide/tui).
- **Reference** — [CLI](/reference/cli), [Configuration](/reference/config),
  [Task file format](/reference/tasks), [Event kinds](/reference/events),
  [Environment](/reference/environment), [Remote API](/reference/remote-api),
  [Architecture](/reference/architecture).
