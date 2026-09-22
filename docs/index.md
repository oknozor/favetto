---
layout: home

hero:
  name: favetto
  text: Remote-TUI-first agent orchestration
  tagline: A long-running, LLM-driven agent orchestrator that schedules and runs declarative tasks by delegating to external coding-agent CLIs — the orchestrator never implements its own agent loop.
  actions:
    - theme: brand
      text: Get started
      link: /guide/getting-started
    - theme: alt
      text: Architecture
      link: /reference/architecture
    - theme: alt
      text: GitHub
      link: https://github.com/oknozor/favetto

features:
  - title: Agents as workers
    details: favetto orchestrates; the coding loop is an external agent CLI embedded as a live PTY session that can be attached, reattached, and keystroke-driven from the TUI.
  - title: Remote-TUI-first
    details: The TUI client and the daemon are decoupled over a single wire protocol; local attach is just a special case of remote attach.
  - title: Event-driven core
    details: Everything is a task, an event, or a hook.
  - title: Declarative tasks
    details: Each task is a Markdown file with a TOML header (agent, plus optional schedule/dependency) and a prompt body.
  - title: Crash-resilient
    details: Tasks and events persist across restarts; the event log is append-only and monotonic, doubling as the TUI's resume cursor.
---

## Quickstart

```bash
cargo build

# terminal 1 — the daemon
./target/debug/favetto daemon

# terminal 2 — the TUI (local attach over the unix socket)
./target/debug/favetto tui

# or attach remotely over WebSocket (token auto-created in ~/.local/share/favetto/token)
./target/debug/favetto tui --remote ws://127.0.0.1:7878 \
    --token-file ~/.local/share/favetto/token

# configure external agents, then start tasks from the TUI
```

Continue with [Getting started](/guide/getting-started), or read the
[architecture overview](/reference/architecture).
