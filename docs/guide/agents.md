# Agents (ACP)

favetto does not implement an agent loop and no longer runs agent CLIs in a
terminal. Every agent is an **Agent Client Protocol (ACP) server**: the daemon
launches it as a child process, speaks JSON-RPC over its stdin/stdout, and
receives structured messages, tool calls and permission requests.

That means there are no per-vendor adapters. Any agent that ships an ACP server
mode can be added with configuration alone. ACP publishes a growing
[list of compatible agents](https://agentclientprotocol.com/get-started/agents).

```bash
# see what is registered, and whether each executable is installed
favetto daemon   # then open the Tasks tab
```

## Configure an agent

The four built-in names are **launch presets**, not implementations:

| Name | Preset |
|---|---|
| `opencode` | `opencode acp` |
| `claude` | `npx -y @zed-industries/claude-agent-acp` |
| `pi` | `npx -y pi-acp` |
| `vibe` | `vibe-acp` |

An empty config exposes all four. A `[agents.<name>]` entry overlays a preset, or
defines any other ACP server:

```toml
[agent]
default = "opencode"

# Any ACP server works; no favetto code is needed.
[agents.goose]
command = "goose"
args = ["acp"]
permission = "ask"     # "ask" (default), "allow", or "deny"
```

The fields are documented exhaustively in the
[configuration reference](../reference/config#agentconfig).

### Permission policy

The permission policy decides how a tool-permission request that **reaches the
client** is answered. It does not force an agent to ask: an agent that
auto-approves tool calls in its own configuration never sends a request.

- `ask` — forward the request to a connected client and wait for an answer. With
  no client attached, it fails closed after a timeout.
- `allow` — approve the request using the agent's `allow_once` option.
- `deny` — reject every request.

Set `permission` on the agent entry as a default, or per task in its header:

```md
agent = "opencode"
permission = "deny"
---
Update the changelog.
```

## MCP servers

Favetto can pass user-installed [MCP](../guide/mcp#mcp-servers-for-agents)
servers to an ACP agent through the protocol's `mcpServers` session field. It
never downloads, installs or supervises them: you register a command (stdio) or a
URL (HTTP/SSE), and the agent connects directly.

Register definitions through `mcp.add` (the app/Settings surface uses the same
RPCs) or seed them from `[[mcp.servers]]` in `config.toml`. Stdio launches
require explicit approval bound to a fingerprint of the command/args; editing
those revokes it. Secret env/header values come from a daemon env var, a secret
command, or a write-only literal stored in a `0600` file, and are never returned
by the RPCs.

Selection is separate from registration:

- `agents.start` / `tasks.start_oneshot` accept `mcp_servers: ["name"]`.
- A catalog task can set `mcp = ["name"]` in its header.
- Absent means every enabled + approved definition; `[]` sends none.

The session's `mcp` field reports the `selected`, `active` and any `warnings`
(e.g. an HTTP server omitted because the agent did not advertise
`mcpCapabilities.http`). A bad entry never blocks the run; an unknown *explicit*
name is an error.

## Authentication

ACP v1 agents can advertise authentication methods during initialization. When
a session needs sign-in before `session/new` or `session/load` can succeed, the
daemon keeps the agent process alive and reports an `auth` state on the session
(`required`, `authenticating`, `failed`, …) instead of failing the launch.

- **Agent-handled methods** are completed by the daemon through ACP
  `authenticate`. The client sends only the advertised method id
  (`agents.authenticate`); it never collects or transports a credential.
- **Terminal methods** are preserved in the advertised list for display but are
  never offered as usable: Favetto's daemon cannot safely bridge an interactive
  login process to a remote TUI/GPUI client. The daemon rejects them explicitly.
  Configure those credentials on the daemon host.
- **Logout** (`agents.logout`) is available only when the agent advertises the
  ACP `logout` capability.

In the TUI Agent panel, **Ctrl+A** opens the sign-in chooser for sessions that
advertise an agent-handled method or logout; the panel shows progress and errors
without printing credentials or raw terminal output.

The GPUI client shows the same state in the task detail and the free-standing
Agent surface. Actions follow the session's auth state rather than the
advertised method list: sign-in is offered only while the session is `required`
or `failed`, and sign-out only while it is `not_required` and the agent
advertises logout. The **Settings → Agents** card is read-only and never exposes
per-session sign-in or sign-out actions; it points at the session surface
instead. After sign-in the daemon retries the paused session setup
automatically.

## Availability

The daemon probes its `PATH` for each agent's executable once at startup and
reports an `available` flag on `agents.list`. An agent whose binary is not
installed is still listed but marked unavailable, and launching it fails with a
clear error. Availability is resolved once, so a `PATH` change needs a daemon
restart.

## The Agent panel

Opening a task shows its ACP session: streamed assistant text, reasoning, tool
calls and errors. Type into the panel to compose a follow-up prompt; **Enter**
sends it as `agents.prompt`. Press **Ctrl+Y** to toggle keyboard focus between
the panel and favetto, **Ctrl+Q** to leave (the session keeps running), and
**Ctrl+N** to start a new session. With favetto focus, **Ctrl+C** interrupts the
current turn through `agents.cancel` without cancelling the task; the session
stays open for a follow-up prompt.

When the agent asks for permission, the panel shows the request. Answer it with
**Ctrl+R**; the reply is correlated with the protocol request, not typed as
terminal bytes.

### Free-standing sessions in the desktop client

The GPUI **Agent** surface (`g a`) starts a session through `agents.start` with
no `task_id`. Its `cwd` is the active repository's `RepoInfo.path` — the
daemon-side path of the main working tree. It is never the daemon cwd and never
a task-generated worktree. With no active repository the panel asks you to pick
one first; it does not silently start elsewhere.

The agent is fixed for an ACP session, so choosing another agent and starting a
new session replaces the active one (the previous session is closed). The
pre-start model field accepts a bare model id or `provider/model`. After start,
model, thinking level and build/plan mode are changed through the agent's
advertised config options and modes (`agents.set_option` / `agents.set_mode`);
the panel does not invent a model selector when the agent advertises none. An
`agent.exit` push leaves the composer disabled and shows the exit state, and
**Close** uses `agents.close`.

## Lifecycle

Agents launch through a small supervisor (`favetto __agent-exec`) that owns a
process group and dies with the daemon — on both `SIGTERM` and `SIGKILL` — taking
the agent and its children with it.

### Transport limits

The stdio transport is newline-delimited UTF-8 JSON-RPC and is bounded:

- A single message may be at most **8 MiB** (excluding the terminating newline).
  A longer line fails the session instead of buffering without bound.
- Invalid UTF-8 and a partial line at EOF (a truncated message) are read errors,
  not lossily repaired or parsed as complete messages.
- `initialize`, `session/new`, `session/load` and model selection share a
  startup deadline, **30 seconds** by default. A hung server fails the session
  with the method and deadline in the error instead of leaving it `Starting` or
  `Loading`. Override the bound with `FAVETTO_ACP_SETUP_TIMEOUT_MS` (milliseconds)
  for an agent with a long cold start.
- Prompt turns (`session/prompt`) are deliberately **not** bounded: a task may
  legitimately run for hours.

Any transport or setup failure kills the agent's whole process group and reaps
the direct child, so a malformed or hung server cannot outlive its session.

A **headless** run is exactly one ACP prompt turn. The session closes itself when
the turn ends, and the task succeeds only when all of the following hold:

- the turn ended with the normal `end_turn` stop reason,
- no protocol or agent error was recorded, and
- the agent produced assistant text.

A `stop_reason` alone is not enough: some adapters report `end_turn` after an
upstream model failure.

An **interactive** session (started from the Agent panel or the one-shot wizard)
stays open for follow-up prompts until it is closed or its process exits.

While a session waits on a permission request, `agents.list` reports its
`awaiting_input` and the Agent panel shows the request. The task row itself stays
`running` until the turn ends; answering with `agents.reply` resumes the turn.

`agents.cancel` interrupts the in-flight turn (`session/cancel`). Every permission
request still pending for that turn is answered with ACP's `cancelled` outcome,
so the turn settles immediately instead of waiting for the permission timeout.
Cancellation is scoped to the current turn: after it settles, a later prompt on
the same session can be cancelled normally. Cancelling while the session is idle
sends nothing to the agent.

## Session history

Favetto records a durable row for every ACP session it creates. The row keeps
Favetto's **local session id**, the configured agent, the working directory, an
optional task link, the agent-reported title, timestamps and lifecycle. The
agent's own ACP session id is stored separately and only the daemon uses it.

There are two different ways to reopen a session, and they are not
interchangeable:

- **Reattach** (`agents.attach`) is for a session whose ACP process is still
  alive in the daemon — or a retained in-process transcript. It never starts or
  loads another process, and keeps the existing Agent-panel behavior. Opening a
  running task follows the same rule: `agents.start` returns the task's live
  session (including an ordinary headless run) instead of creating a duplicate,
  and the client backfills its feed with `tasks.events`.
- **Load** (`agents.load`) restores an ended, provider-persisted session by its
  **Favetto-local id**. The daemon looks up the recorded ACP id and asks the
  agent for `session/load`; the history replay is applied before the session is
  reported ready. A load never sends a prompt and never falls back to
  `session/new`.

The TUI's **Ctrl+O** picker lists both kinds. A live row attaches; an ended,
loadable row previews its local id, agent, title, directory and state, then
loads when selected. Rows that are loading, failed, or cannot be loaded explain
why and do nothing rather than opening a blank session.

The desktop client does not enumerate history. Opening a task issues one
indexed, task-scoped `tasks.session` lookup; a live associated session is
attached and an ended, loadable one is restored once with `agents.load`. The
replayed conversation appears in the task detail's Output surface, labelled with
its Favetto-local id, agent and state, and a finished run stays finished: the
restore never starts a session, sends a prompt, retries the task or changes its
status/output.

Restoring is durable across daemon restarts and store reopens. The daemon looks
up the stored ACP id from its own record, passes it to `session/load`, and keeps
the Favetto-local id as the only routing key; the provider id is never returned
by `agents.history`, and `agents.load` accepts only the local id. A successful
restore applies the whole replay before the session becomes ready, keeps the
replayed transcript across reconnects, and accepts follow-up prompts. Loading a
session does not send a prompt, run its linked task, create a new run, or change
an interrupted run's status.

`agents.history` lists only sessions **created by Favetto**. Provider-owned
history is separate and is queried from the agent itself with ACP
`session/list` through `agents.provider_sessions`; the daemon only issues that
request when the agent advertises `sessionCapabilities.list`. It supports the
provider's `cwd` filter and opaque cursor pagination. A provider result is never
merged into Favetto's durable records.

A provider session is imported into a durable Favetto record **only when it is
resumed**. `agents.provider_resume` takes the configured agent and the provider's
own ACP session id, prefers `session/load` when `loadSession` is advertised (so
the replay is applied before readiness), and otherwise uses `session/resume`
when the agent advertises `sessionCapabilities.resume`. The result carries the
new Favetto-local id; all later `agents.attach`, `agents.load`, prompts and
closes use that local id. Resuming the same provider session again reuses its
imported local record instead of creating a duplicate.

Provider transcript deletion is capability gated too: `agents.provider_delete`
calls ACP `session/delete` only when `sessionCapabilities.delete` is advertised.
It deletes the provider transcript only — Favetto's local metadata is a distinct
lifecycle and stays in `agents.history`. Deleting an active provider transcript
is refused; close its local session first.

The TUI **Ctrl+O** picker shows both local and provider rows. Provider rows are
labelled `provider`; they can be resumed when load/resume is advertised and
deleted (Ctrl+D in the TUI) when delete is advertised. Unsupported actions are
absent or disabled with an inline explanation. The desktop client does not list
either kind: it restores only the session associated with the task being opened.

Whether a local past session is loadable also depends on the agent advertising
the ACP `loadSession` capability. An unsupported capability, an agent-reported
`session/load` error, or an initialization failure is reported as a failed
restore, with the session still listed and any partial replay discarded. A
failed restore can be retried from the clients; it never falls back to a fresh
session.

Conversation transcript ownership stays with the ACP agent. Favetto stores
metadata only; the bounded, prunable task output/event feed is not a transcript
and is never used as one when loading a past session.

## Current limitations

- Model selection uses the agent's ACP **model config option**: a task's
  `provider`/`model` is sent as `provider/model` and the run fails with a clear
  error if the agent does not offer it. Live sessions also expose their complete
  ACP config-option and mode snapshots to clients; the TUI opens the picker with
  `Ctrl+S` in the Agent panel and GPUI renders the same controls above the task
  detail tabs. The provider catalog (`providers.list`) is still empty; model
  choices come from the session's ACP options.
- Sessions can be restored across daemon restarts only when both Favetto's
  durable record and the agent's own persisted session survive, and the agent
  advertises ACP `loadSession`. A session created entirely outside Favetto is
  not discoverable or importable.
- The GPUI task detail shows a bounded (8,000-character) preview of the restored
  conversation; open the session from the TUI to read the full replay. A session
  whose daemon died before it recorded an ACP id remains `loading` and is not
  loadable.
- The provider catalog (`providers.list`) is empty: ACP agents expose model
  choices through session options rather than a catalog.
