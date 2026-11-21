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
**Ctrl+N** to start a new session.

When the agent asks for permission, the panel shows the request. Answer it with
**Ctrl+R**; the reply is correlated with the protocol request, not typed as
terminal bytes.

## Lifecycle

Agents launch through a small supervisor (`favetto __agent-exec`) that owns a
process group and dies with the daemon — on both `SIGTERM` and `SIGKILL` — taking
the agent and its children with it.

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
why and do nothing rather than opening a blank session. The web Settings page
and the desktop **Settings → Agents → Past sessions** card use the same
`agents.history` / `agents.load` contract. The desktop card shows the restored
transcript under a row only after `agents.load` succeeds; while the replay is in
flight the row reads `loading` and no transcript is shown.

Restoring is durable across daemon restarts and store reopens. The daemon looks
up the stored ACP id from its own record, passes it to `session/load`, and keeps
the Favetto-local id as the only routing key; the provider id is never returned
by `agents.history`, and `agents.load` accepts only the local id. A successful
restore applies the whole replay before the session becomes ready, keeps the
replayed transcript across reconnects, and accepts follow-up prompts. Loading a
session does not send a prompt, run its linked task, create a new run, or change
an interrupted run's status.

Only sessions **created by Favetto** are listed. Importing arbitrary sessions
from a provider via ACP `session/list` is out of scope; whether a past session is
loadable also depends on the agent advertising the ACP `loadSession`
capability. An unsupported capability, an agent-reported `session/load` error,
or an initialization failure is reported as a failed restore, with the session
still listed and any partial replay discarded. A failed restore can be retried
from the clients; it never falls back to a fresh session.

Conversation transcript ownership stays with the ACP agent. Favetto stores
metadata only; the bounded, prunable task output/event feed is not a transcript
and is never used as one when loading a past session.

## Current limitations

- Model selection uses the agent's ACP **model config option**: a task's
  `provider`/`model` is sent as `provider/model` and the run fails with a clear
  error if the agent does not offer it. The provider catalog
  (`providers.list`) is still empty, so the UI has no model picker.
- Sessions can be restored across daemon restarts only when both Favetto's
  durable record and the agent's own persisted session survive, and the agent
  advertises ACP `loadSession`. A session created entirely outside Favetto is
  not discoverable or importable.
- The GPUI **Past sessions** card shows a bounded (4,000-character) preview of
  the restored transcript; open the session from the TUI to read the full
  replay. A session whose daemon died before it recorded an ACP id remains
  `loading` and is not loadable.
- The provider catalog (`providers.list`) is empty: ACP agents expose model
  choices through session options rather than a catalog.
