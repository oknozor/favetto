# Architecture

favetto is a long-running, LLM-driven agent orchestrator with a
**remote-TUI-first** design. It schedules and runs declarative tasks by delegating
to external coding-agent CLIs (opencode, Claude Code, pi, Mistral Vibe, …),
embedded as live terminals — the orchestrator never implements its own agent loop.

```
┌───────────────────────────────────────────────────────────────┐
│                  TUI Client (ratatui)                         │
│   local attach  ─► unix:///tmp/favetto.sock              │
│   remote attach ─► ws://host:7878/rpc?token=…                 │
│   (same MessagePack wire protocol over both)                  │
└───────────────────────────────────────────────────────────────┘
                             │
┌───────────────────────────────────────────────────────────────┐
│                  Favetto Daemon                          │
│   Scheduler ─► Task Queue ─► Agent Sessions (PTY) ─► Agent CLIs│
│   Event Bus · Hook Engine · Notification Bus                  │
│   Persistence (SQLite/sqlx) · Remote API (axum)               │
└───────────────────────────────────────────────────────────────┘
```

## Design principles

- **Agents as workers** — favetto orchestrates; the coding loop is an external
  agent CLI embedded as a live PTY session that can be attached, reattached, and
  keystroke-driven from the TUI.
- **Remote-TUI-first** — the TUI client and the daemon are decoupled over a single
  wire protocol; local attach is just a special case of remote attach.
- **Event-driven core** — everything is a task, an event, or a hook.
- **Declarative tasks** — each task is a Markdown file with a TOML header
  (`agent`, plus optional schedule/dependency) and a prompt body.
- **Crash-resilient** — tasks and events persist across restarts; the event log is
  append-only and monotonic, doubling as the TUI's resume cursor.

## Workspace layout

```
crates/
  favetto-core/          shared wire protocol + domain types (no HTTP/DB/LLM)
  favetto-providers/     provider/model catalog (opencode auth + models.dev)
  favetto-tui/           dependency-light terminal client binary
  favetto/               daemon binary + doc generator
tasks/                        task catalog: `*.md` files with TOML header + prompt
docs/                         VitePress site: guide + generated reference
```

## Features

| Area | Scope |
|------|-------|
| **Core + remote TUI** | daemon, SQLite, event bus, MessagePack wire protocol over Unix socket + WebSocket, token auth, ratatui client |
| **Agent sessions** | PTY spawn, server-side `vt100` emulation, remote attach/reattach, terminal query replies |
| **Scheduler + notifications** | cron, task queue → agent sessions, notification channels, Scheduler + Notifications TUI tabs |
| **Integrations + hardening** | metrics, pairing, webhook receivers + config-driven rules, parallel git-worktree execution |

## Wire protocol

The same `Frame` type is carried over the Unix socket and the WebSocket
transport, which is what makes local attach just a special case of remote attach.
Each frame is MessagePack-encoded and tagged with a `type`:
`request` / `response` / `notification`. See the
[Remote API reference](./remote-api) for the full method list.

## Supervisor boundary

An external supervisor — a script, a binary, a web UI, a human, or another
agent — can drive a workflow by observing it with `workflow.inspect` (plus the
event cursor from `events.subscribe`) and submitting a strict vocabulary of
decisions through the workflow RPCs. The supervisor is a plain remote API
client; it is never part of the daemon.

This keeps the boundary above intact: favetto validates and executes, while all
judgment — which task to spawn, when to retry, when to stop — stays outside.
There is no LLM or agent loop in the daemon, and the daemon stores no supervisor
state. The full observation and decision schema and the action-to-RPC mapping
live in the [Supervisor contract reference](./supervisor-contract). An
MCP-speaking supervisor ships as the `favetto-mcp` binary; see the
[MCP supervisor guide](../guide/mcp).

Because the daemon is long-running, its production panic paths are audited and
mechanically linted, and its persisted output is bounded; see
[Panic safety](../design/panic-safety) for the inventory and the guard tests.
