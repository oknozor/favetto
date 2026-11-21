# ACP migration plan

Status: implemented (ACP cutover complete)
Scope: Favetto daemon, agent runtime, wire API, and clients.
Goal: retain MessagePack for Favetto daemon interactions while replacing
PTY-backed agent sessions with a generic ACP runtime.

ACP is the protocol between an agent and its client. It does not replace
Favetto's daemon API: task orchestration, workflows, schedules, repositories,
configuration, authentication, and persistence remain Favetto responsibilities
and continue to use the existing MessagePack RPC.

---

## 1. Goals and non-goals

### Goals

1. Run ACP-compatible agents as supervised child processes communicating over
   ACP's JSON-RPC transport, rather than interpreting their terminal screens.
2. Support both interactive conversations and unattended task runs.
3. Stream structured agent updates to Favetto clients and persist useful
   structured task results.
4. Route agent permission requests through Favetto's policy and user interface.
5. Keep MessagePack as the client-to-daemon transport while evolving its API
   freely in coordinated changes.
6. Keep the agent runtime independent of whether the connected UI is Dioxus,
   GPUI, or another Favetto client.
7. Use one generic ACP implementation and command configuration to unlock the
   broader ACP agent ecosystem, rather than implementing a Favetto adapter for
   each vendor.

### Non-goals

- Replacing Favetto's MessagePack RPC or event stream with ACP.
- Rewriting the scheduler, workflow engine, persistence, or task definition
  format.
- Making a terminal-only CLI ACP-compatible without an ACP server or a
  community/upstream ACP adapter.
- Extracting Zed's built-in agent implementation as part of the initial
  transport migration. ACP does not itself expose that implementation.
- Keeping PTY as a permanent compatibility backend. PTY may be used temporarily
  during migration, but it is not the target architecture.

---

## 2. Current state and target

Favetto currently starts configured CLIs in PTYs. `AgentManager` owns the
process, `vt100` screen, input writer, and output frames. The daemon sends
base64-encoded terminal frames to clients through `agent.output`; clients send
raw bytes through `agents.input`. A parallel state-source seam normalizes
structured observations into `AgentStateEvent`s.

The target architecture is:

```text
Favetto UI ── MessagePack RPC + SSE ── Favetto daemon ── ACP / stdio ── Agent
                                             │
                                   task/workflow runtime
```

The daemon is the ACP client. It launches and supervises the agent, handles
protocol messages, and translates them into Favetto's session and task model.
The UI never speaks ACP directly; it continues to use authenticated,
remote-capable Favetto APIs.

ACP sessions have structured messages and prompt turns rather than a terminal
screen. The ACP path must not synthesize ANSI frames as its source of truth.
The existing `agent.output` path and PTY-specific RPCs are transitional and
should be removed once the ACP client and task workflows have cut over.

### Existing seams to build on

| Concern | Location |
|---|---|
| Agent invocation abstraction | `crates/favetto/src/agents/agent.rs` |
| PTY session manager | `crates/favetto/src/agents/mod.rs` |
| Normalized agent state | `crates/favetto-wire/src/agent_state.rs` |
| Agent state adapters | `crates/favetto/src/agents/state.rs` |
| RPC types and method registry | `crates/favetto-wire/src/rpc/` |
| RPC dispatch and pushes | `crates/favetto/src/server.rs` |
| Headless task execution | `crates/favetto/src/executor.rs` |
| Existing adapter plan | `docs/design/agent-state-adapters.md` |

The existing `AgentStateEvent` and `agent.event` push already cover several
useful concepts, including text deltas, tool activity, usage, input requests,
and turn outcomes. Reuse those types where their semantics match ACP; extend
extend the wire types when important ACP data cannot be represented.

---

## 3. Proposed runtime architecture

Replace the PTY-oriented `AgentManager` with an ACP session manager. During
development, the old manager may remain available behind a temporary migration
switch, but the long-term runtime should not maintain two agent transports:

```text
ACP session manager
  └── generic ACP process: command + args + environment + JSON-RPC + events
```

Keep the ACP implementation separate from PTY-specific concerns. An ACP
session manager should own:

- Child process lifecycle, stdin/stdout protocol I/O, and stderr diagnostics.
- ACP initialization and negotiated capabilities.
- ACP session IDs, Favetto session IDs, and their mapping to task IDs.
- Prompt turns, cancellation, session close, and supported resume/load behavior.
- Event delivery and permission-request correlation.
- Cleanup on task cancellation, daemon shutdown, and process failure.

Protocol stdout must be reserved for ACP messages. Logs belong on stderr.
Requests and notifications must be correlated and parsed independently of
arbitrary pipe chunk boundaries. Apply size limits and timeouts to protocol
messages and startup/initialization.

Configure ACP agents generically using the executable, arguments, and
environment required by their documented ACP server mode. Favetto should not
need vendor-specific launch, JSONL, session-ID, or provider parsers for an agent
that correctly implements ACP. Authentication and model selection remain with
the agent unless it exposes them through negotiated ACP capabilities or
configuration.

The [ACP agent list](https://agentclientprotocol.com/get-started/agents) is
broad and includes agents and adapters such as OpenCode, Claude Agent, Codex
CLI, Gemini CLI, Mistral Vibe, and Pi. Treat it as a discovery list rather than
a blanket guarantee: ACP support and optional capabilities vary by agent and
version. Maintain a smoke-tested support matrix, but do not turn each entry
into a Favetto-specific implementation.

---

## 4. Favetto API and event model

Keep MessagePack as the client-to-daemon protocol. Replace PTY-specific
operations with structured methods for ACP interactions. The exact method
names and payloads should be settled in the first implementation RFC, but the
API needs to represent:

- Sending a prompt to an existing session.
- Cancelling an in-flight turn.
- Returning a permission decision correlated with the ACP request.
- Optionally setting negotiated session options or modes.

Keep existing session start, attach, close, and listing semantics where they
still apply. Replace `agents.input` and `agents.resize` with prompt, cancel,
permission-response, and negotiated-session-option operations. Remove
PTY-specific operations after the migration window rather than preserving them
as a second permanent API.

For ACP pushes:

1. Normalize ACP message, tool, usage, and lifecycle updates into the existing
   `AgentStateEvent` model where possible.
2. Extend the wire model for ACP content that Favetto needs but cannot currently
   express (for example, a structured plan or additional content-block kinds).
3. Use the existing session-scoped event push/backfill pattern where possible.
   Preserve ordering and cursors across reconnects.
4. Keep task status and final run output durable in Favetto's existing
   persistence; live ACP events remain live session state unless explicitly
   persisted.

The wire API remains the boundary between UI and daemon. A GPUI frontend should
consume the same API as Dioxus and must not need ACP-specific process logic.

---

## 5. Permission, filesystem, and terminal policy

The daemon is the ACP client, so ACP client requests arrive at the daemon rather
than directly at the UI. Define and test the trust model before enabling
unattended ACP agents.

- **Permission requests:** apply configured policy when safe and supported;
  otherwise publish a correlated request to connected clients and wait for a
  user decision. Define behavior when no UI is connected, a request times out,
  or the task is cancelled. Do not silently approve by default.
- **Filesystem access:** decide whether the daemon, the agent, or both own file
  reads and writes. Validate paths against the task's effective workspace and
  worktree policy before exposing client-side filesystem capabilities.
- **Terminal access:** decide whether to expose ACP terminal operations through
  a Favetto-managed terminal service. Avoid giving an agent an untracked shell
  path that bypasses Favetto's process supervision, worktree, or cancellation
  policies.
- **Authentication and secrets:** pass only the environment and credentials
  needed by the selected agent. Keep secrets out of RPC payloads and logs.
- **Remote UI disconnection:** define whether pending permission requests
  remain available for reconnect, fail closed, or follow an explicit policy.

ACP capabilities are negotiated. Do not advertise a client capability until
Favetto implements its semantics and security checks.

---

## 6. Migration phases

### Phase 0 — Generic ACP capability spike

Build a small generic ACP process client and validate it against at least two
independent ACP servers from the compatibility list. This tests the protocol
implementation rather than coupling the proof of concept to one vendor.

Validate:

- Process launch and ACP initialization.
- A one-shot task using a Favetto worktree.
- Interactive prompt, streamed text/tool updates, a follow-up prompt, and
  cancellation.
- Permission request round trip.
- Authentication, model selection, and session resume if required.
- Behavior on malformed messages, child exit, timeout, and UI disconnect.

Inventory current built-ins and custom-agent use. For each, record whether it
has a native ACP server, a listed community adapter, or no viable ACP path.
Confirm server launch instructions, authentication requirements, platforms,
and optional ACP capabilities. Decide which existing agents can be retired if
they have no ACP implementation.

**Exit criteria:** the same generic Favetto ACP implementation can complete a
representative headless task and interactive turn against multiple servers
without terminal-screen parsing; protocol gaps and the initial support matrix
are documented.

**Status: spike complete** (`spikes/acp`, a standalone crate outside the
Cargo workspace). Results, measured on this machine:

| Server (launch command) | Init | Session | Streamed text | Tool calls | Permission request | Cancel | Outcome |
|---|---|---|---|---|---|---|---|
| OpenCode 2.2.7 (`opencode acp`) | yes | yes | yes | yes (`write`, `read`) | yes, when `permission.edit = ask`; denied → tool `Failed`, turn `Cancelled`; `--yolo` → approved | yes → `Cancelled` | Full pass |
| Mistral Vibe 2.25.7 (`vibe-acp`) | yes | yes | yes | yes (`Writing hello.txt`) | not requested in default config (agent auto-approved the write) | not run | Full pass for prompt and tools |
| Claude Agent adapter 0.23.1 (`npx @zed-industries/claude-agent-acp`) | yes | yes | — | — | — | — | Blocked by account credit ("Credit balance is too low"), reported as a JSON-RPC `Internal error` |
| Pi adapter 0.0.34 (`npx pi-acp`) | yes | yes | no model output | no | — | — | Blocked by provider balance (`insufficient_balance_error`); adapter still returned `stop_reason=EndTurn` |

Findings that change the plan:

1. **The generic client works without per-agent code.** The same binary drove
   OpenCode, Vibe, and two adapters. Vendor differences were limited to launch
   command and environment.
2. **Permission requests depend on the agent's own policy.** Favetto cannot
   assume a `session/request_permission` will arrive before a tool runs. The
   fail-closed handler only matters for agents configured to ask.
3. **`stop_reason` is not a reliable success signal.** Pi's adapter reported
   `EndTurn` when the model call had failed. Favetto must combine the stop
   reason with error notifications, tool outcomes, and output checks before
   marking a task succeeded.
4. **Upstream provider failures arrive in different shapes**: JSON-RPC errors
   (Claude adapter) or silent completion (Pi adapter). The mapping in Phase 1
   needs both.
5. **Cancellation works** through `session/cancel` as a notification, and the
   pending prompt resolves with `Cancelled`.

Not yet validated: session resume (`load_session` is advertised by all four but
not exercised), follow-up prompts within one session, MCP forwarding, and
terminal/filesystem client capabilities (none are advertised by the spike, so
agents use their own).

Reproduce:

```sh
cd spikes/acp && cargo build
./target/debug/acp-spike --yolo --command "opencode acp" --cwd /tmp/acp-ws "Create hello.txt"
./target/debug/acp-spike --command "vibe-acp" --cwd /tmp/acp-ws --cancel-after 6 "…"
```

### Phase 1 — ACP wire and normalized event contract

- Specify request/response payloads for prompt, cancel, permission response,
  and any required session options.
- Map ACP session and turn lifecycle to Favetto session/task lifecycle.
- Reuse existing normalized events where appropriate; add compatible event
  variants for missing information.
- Specify ordering, replay/backfill, and behavior for unknown ACP update kinds.
- Add serialization and client/daemon contract tests for the new shapes.
  Backward-compatibility shims and tests are not required.

**Exit criteria:** the contract is documented and testable independently of a
particular ACP process implementation.

### Phase 2 — ACP process and session runtime

- Implement the ACP subprocess connection and supervised child lifecycle.
- Add initialization/capability negotiation and per-session routing.
- Stream normalized events into the daemon session model.
- Implement cancellation, close, child-exit handling, and bounded protocol
  parsing.
- Keep the old PTY path only as a temporary, explicitly marked migration path.
  Do not add new PTY-only agent integrations while the ACP cutover proceeds.

**Exit criteria:** transport tests cover fragmented messages, concurrent
requests, process failure, cancellation, and cleanup; multiple independent ACP
servers pass end-to-end session tests.

### Phase 3 — RPC dispatch and client integration

- Add structured prompt/cancel/permission RPC operations.
- Forward ACP events over the existing authenticated event channel and support
  reconnect/backfill.
- Update Dioxus to render ACP conversation and tool state and to return
  permission decisions.
- Keep any temporary PTY bridge isolated behind a migration flag; do not expose
  PTY-specific controls as part of the target client API.
- Add the same contract tests for any GPUI client; do not add ACP handling to
  the UI process.

**Exit criteria:** ACP sessions work remotely through the daemon, including
permissions and reconnection, without clients needing an ACP connection.

### Phase 4 — Task executor integration

- Run headless task prompts through ACP prompt turns.
- Map ACP stop reasons, errors, and cancellation into Favetto run status.
- Persist a useful structured run summary and usage where supplied.
- Preserve retry, timeout, worktree cleanup, and dependency/spawn behavior.
- Test that a task does not remain `running` after a completed ACP turn even if
  the ACP process stays alive for the session.

**Exit criteria:** scheduler-driven and manual tasks pass lifecycle and
workflow tests with an ACP backend.

### Phase 5 — ACP ecosystem rollout and PTY removal

- Add documented launch configurations for ACP servers. Prefer configuration
  over source-code adapters; implement an agent-specific adapter only when
  required to bridge a concrete protocol gap and keep it outside the generic
  ACP runtime.
- Roll out the generic ACP backend behind a short-lived feature flag, exercise
  the smoke-test matrix, then make ACP the only supported runtime.
- For agents without an ACP server or community adapter, either contribute or
  maintain a separate ACP adapter, or declare them unsupported. Do not silently
  fall back to PTY as a permanent compatibility promise.
- Remove `portable-pty`, `vt100`, screen heuristics, terminal-frame pushes, and
  PTY-only RPCs once the ACP path covers required interactive and task flows.

**Exit criteria:** the supported-agent matrix is smoke-tested, all required
workflows use ACP, and PTY-specific runtime and API code is removed.

---

## 7. Testing and acceptance

### Unit and protocol tests

- ACP initialization and capability negotiation.
- JSON-RPC request correlation, notifications, malformed messages, and unknown
  update kinds.
- ACP-to-Favetto event mapping and stop-reason mapping.
- Permission policy, correlation IDs, timeouts, and fail-closed behavior.
- Session ID mapping and duplicate/out-of-order notification handling.

### Runtime and integration tests

- Fake ACP server for deterministic tests of prompt, updates, permissions,
  cancellation, session close, and process exit.
- Real ACP-server smoke test in a separate optional CI job where credentials
  and installation permit.
- Daemon restart/shutdown and task timeout must not leak child processes.
- Reconnect must not silently lose required durable task results; live event
  backfill must respect its documented window.
- ACP tasks must preserve workflow dependencies, retries, worktree cleanup,
  and task cancellation behavior.

### Client acceptance

- Users can read streamed assistant and tool updates.
- Users can answer or reject permission requests.
- Users can send a follow-up prompt, cancel a turn, and reconnect to a session
  according to negotiated capabilities.
- PTY-specific controls are not shown for ACP sessions.
- Existing non-agent Favetto workflows continue using the same MessagePack
  contract.

---

## 8. Risks and open decisions

1. **Agent coverage:** the ACP ecosystem is broad, but not every agent in
   Favetto's current built-in list necessarily exposes ACP. Decide which
   unsupported agents to drop versus support through an external ACP adapter.
2. **Security boundary:** Favetto must define permission, filesystem, and
   terminal policies because the daemon is the ACP client and often runs away
   from the user's UI.
3. **Protocol/model mismatch:** ACP carries structured content that may exceed
   the existing `AgentStateEvent` shape. Avoid lossy mapping of information
   needed by clients or task reports.
4. **Session persistence:** ACP resume support is optional; define behavior for
   agents that cannot load sessions after daemon restart.
5. **Headless execution:** model a prompt turn as task execution, not process
   lifetime. An ACP server may remain alive after a turn finishes.
6. **Pre-alpha API changes:** backward compatibility is not a requirement.
   Update the daemon, Dioxus/GPUI clients, and tests in coordinated changes;
   remove obsolete PTY methods and event shapes rather than retaining shims.
7. **Zed native agent:** using ACP does not make Zed's built-in agent available.
   Running or extracting Zed's `NativeAgent` would be a separate project with
   substantial dependencies and a GPL-3.0-or-later licensing review.

---

## 9. Recommended first implementation

Start with Phase 0 and implement a minimal generic ACP subprocess client tested
against multiple independent agents. Use the spike to settle permission
policy, session semantics, capability handling, and the minimum wire changes.
Then cut interactive and headless flows over to ACP, update the client event
model, and remove PTY-specific runtime/API code as soon as the required
workflows pass.

MessagePack remains the stable client-to-daemon protocol throughout. The ACP
server command, arguments, environment, and negotiated capabilities—not a
Favetto-maintained per-agent implementation—should be the normal way to add
another compatible agent.

---

## 10. Implementation status

The cutover is complete in the daemon:

- **ACP only.** `portable-pty` and `vt100` are gone from the daemon; the agent
  manager owns ACP child processes, transcripts and structured events. The
  per-vendor adapters (opencode, Claude, pi, Vibe, the template-only
  `configurable` agent), the screen heuristics (`attention`, `detect`), the
  state-source seam and the Claude hook receiver were deleted.
- **Generic configuration.** `[agents.<name>]` is `command` + `args` + `env` +
  `cwd` + `permission` + optional git overrides. The four built-in names are
  launch presets only.
- **Task-level permission policy.** `permission = "ask" | "allow" | "deny"` is
  parsed from the task header, overridable per agent entry.
- **Strict task success.** A headless run succeeds only on `end_turn`, no
  recorded error, and non-empty assistant output. This closes the pi-acp
  reported-`end_turn`-after-provider-failure hole found in the spike.
- **Structured client transport.** `agents.prompt` and `agents.cancel` replace
  `agents.input`/`agents.resize`; `agent.output` (base64 terminal frames) is
  gone. Clients render `agent.event` and `agents.attach` returns the transcript.
- **Clients.** The TUI renders structured events into its agent panel and sends
  composed prompts on Enter; the web client uses the new typed calls.

Per-task model selection is implemented through ACP's `session/set_config_option`
(the agent's model selector), so a task's `provider`/`model` is applied
deterministically instead of relying on the agent's last-used default.

Remaining follow-ups (tracked in the guide's "Current limitations"):
`session/load` resume, a provider/model catalog for the UI picker, and richer
session modes plus identified/Markdown plan operations in the normalized event
model. Baseline v1 plan checklists and advertised slash commands are already
normalized and rendered in both clients.
