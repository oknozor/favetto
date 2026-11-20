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

## Current limitations

- Model selection uses the agent's ACP **model config option**: a task's
  `provider`/`model` is sent as `provider/model` and the run fails with a clear
  error if the agent does not offer it. The provider catalog
  (`providers.list`) is still empty, so the UI has no model picker.
- Sessions are not resumed across daemon restarts; ACP's optional
  `session/load` is not wired up yet.
- The provider catalog (`providers.list`) is empty: ACP agents expose model
  choices through session options rather than a catalog.
