# ACP implementation completeness review

**Reviewed:** 2026-11-21  
**Scope:** Favetto daemon ACP client and the ACP-facing behavior exposed to its clients.  
**Protocol baseline:** Official ACP v1 documentation. ACP v2 is reviewed separately as a Preview, not treated as a v1 conformance requirement.

## Executive summary

Favetto has a substantive, generic **ACP v1 client implementation**, not merely a launch wrapper. Its main happy path is present: it launches an agent over stdio, initializes a v1 connection, creates or loads a session, sends text prompts, streams common message/tool updates, routes permission requests through policy/UI, supports cancellation and close, and sets a requested model through a session config option.

It is **not a complete implementation of all stable ACP v1 features**. Many optional v1 features are not negotiated or handled (authentication, elicitation, client filesystem/terminal services, modes, identified/Markdown plan operations, subagents, and provider-side session discovery/deletion). That is often a deliberate and valid subset: a client must not advertise capabilities it cannot provide, and Favetto generally advertises none. There are also concrete v1 gaps that are more than optional feature omissions:

1. `session/cancel` does not resolve outstanding permission requests with the required `cancelled` outcome, and the runtime appears to send that notification only once per connection.
2. The runtime sends optional `session/close` without checking that the agent advertised close support.
3. ACP `usage_update` cost is treated as USD regardless of its currency and is accumulated as a delta even though ACP reports cumulative session state.
4. Common message content other than text is discarded rather than retained or displayed as a placeholder.

**Overall assessment:** solid v1 core / incomplete v1 feature coverage, with correctness issues in cancellation, optional close negotiation, and usage mapping. **ACP v2 support: none.** The implementation imports `agent_client_protocol::v1`, requests `ProtocolVersion::V1`, and uses v1 session and prompt semantics.

## Review method and scope

The ACP reference was reviewed from the official documentation index, introduction and architecture pages, and the complete protocol/v1 page set: overview, initialization, authentication, session setup/list/delete, prompt turn, content, tool calls, agent plan, elicitation, filesystem, terminals, config options, modes, slash commands, subagents, transports, request cancellation, errors, extensibility, security, and schema. The v2 Preview overview, migration guide, and corresponding protocol pages were also reviewed to identify version-specific differences.

The implementation review focused on:

- `crates/favetto/src/agents/acp.rs` — process transport, lifecycle, request handling, permissions, and event mapping.
- `crates/favetto/src/agents/mod.rs` — session manager, event folding, client-facing lifecycle and usage.
- `crates/favetto-wire/src/agent_state.rs` — normalized event/content/usage model.
- [`docs/guide/agents.md`](../guide/agents.md) and [`docs/design/acp-migration.md`](acp-migration.md) — documented product scope and known limitations.
- The deterministic fake agent and ACP-focused tests, as evidence of exercised paths.

This is a source audit, not a conformance-test-kit run or a fresh interoperability test against every ACP server. ACP is bidirectional: Favetto is the **Client** role on the ACP connection. It is not expected to implement the Agent-side language-model/tool loop or every optional client capability. The UI-to-daemon MessagePack/SSE API is Favetto's own protocol and is outside ACP conformance except where it affects ACP behavior.

## Feature-by-feature v1 comparison

Status meanings:

- **Implemented** — there is an identifiable code path for the feature's main protocol behavior.
- **Partial** — the core path exists, but relevant protocol data/semantics are lost or incomplete.
- **Not implemented / not advertised** — Favetto does not offer the optional feature to agents. This is generally safe if the corresponding capability is omitted.
- **Issue** — implementation behavior conflicts with a v1 requirement or capability rule.

| Official v1 page / feature | Status | Favetto implementation and gap |
|---|---|---|
| [Overview](https://agentclientprotocol.com/protocol/v1/overview) | **Implemented, scoped** | Favetto is the ACP Client. The daemon owns agent subprocesses and converts ACP updates to Favetto events; Favetto UI clients do not speak ACP directly. The local-agent stdio flow is the supported deployment; ACP's developing remote transports are not used. |
| [Transports](https://agentclientprotocol.com/protocol/v1/transports) | **Implemented, with hardening gaps** | Child stdin/stdout are connected using newline-delimited JSON-RPC; stderr is drained separately; stdin writes include a newline. The process is supervised and killed on shutdown. `lossy_lines` replaces invalid UTF-8 rather than rejecting it, and this layer has no explicit maximum line/message size or initialization timeout. These are robustness/interoperability risks; ACP requires UTF-8 and the migration plan itself calls for bounded parsing/timeouts. |
| [Initialization](https://agentclientprotocol.com/protocol/v1/initialization) | **Partial but valid minimal negotiation** | Sends `initialize` with protocol v1 and records agent capabilities. No client capabilities are advertised (therefore filesystem, terminal, elicitation, plan operations, notices, compaction, subagents, and boolean config are not enabled); `clientInfo` is omitted. Plain `plan` checklists and `available_commands_update` are baseline v1 updates and do not require a capability. The negotiated version and most capability groups are not used to enable corresponding features. `loadSession` is checked before `session/load`. See `acp.rs` initialization around lines 422–430. |
| [Authentication](https://agentclientprotocol.com/protocol/v1/authentication) | **Not implemented** | The client does not implement `authenticate`, terminal login/reconnect, or `logout`; it also does not offer terminal-auth capability. An agent that requires auth must already be authenticated by the operator/environment or its setup fails. Favetto should continue to omit auth capability rather than imply support. |
| [Session setup](https://agentclientprotocol.com/protocol/v1/session-setup) | **Core implemented; setup options limited** | Creates sessions with `cwd`; loads a persisted session only when `loadSession` is advertised; load replay is buffered until success and applied before readiness. Favetto tracks its own local ID separately from the provider ACP ID. It sends no configured MCP server list, `additionalDirectories`, or `systemPrompt`; those are unsupported client features rather than falsely advertised capabilities. Provider-owned sessions can be resumed through `agents.provider_resume`, which prefers `session/load` and falls back to `session/resume` when advertised. See `acp.rs` around lines 450–570 and 572–611. |
| [Session list](https://agentclientprotocol.com/protocol/v1/session-list) | **Implemented, capability gated** | `agents.provider_sessions` launches a short-lived ACP connection and calls `session/list` only when `sessionCapabilities.list` is advertised. It supports `cwd` filtering and opaque cursor pagination and returns provider rows separately from Favetto's durable `agents.history` records. |
| [Session delete](https://agentclientprotocol.com/protocol/v1/session-delete) | **Implemented, capability gated** | `agents.provider_delete` calls optional ACP `session/delete` only when `sessionCapabilities.delete` is advertised. Favetto's local metadata/history lifecycle remains separate; provider transcript deletion never removes local records. |
| [Session resume](https://agentclientprotocol.com/protocol/v1/session-setup) | **Implemented for provider sessions** | `agents.provider_resume` imports a provider-owned session only on resume, preferring `session/load` (with the existing replay gate) when `loadSession` is advertised, then `session/resume` when `sessionCapabilities.resume` is advertised. The Favetto-local id remains the client routing key. |
| [Prompt turn](https://agentclientprotocol.com/protocol/v1/prompt-turn) | **Core implemented; content partial** | Sends one text `ContentBlock` per Favetto prompt, awaits the v1 prompt response, handles follow-up prompts, maps stop reasons to idle outcomes, and requires `end_turn`, no error, and non-empty assistant text for headless success. User/assistant/thought text chunks are mapped. Other message content blocks do not make it through `content_text`; prompt composition is text-only. |
| [Content](https://agentclientprotocol.com/protocol/v1/content) | **Partial** | Tool-call content maps text, image/audio metadata, resource links/resources, diffs and terminal references into Favetto wire types; binary payloads are intentionally not forwarded. However, image/audio/resource blocks in user/assistant/thought message chunks are dropped, and unknown tool content is dropped. The normalized message event only carries text, so clients cannot preserve or show placeholders for non-text message content. |
| [Tool calls](https://agentclientprotocol.com/protocol/v1/tool-calls) | **Mostly implemented; cancellation caveat** | Tool starts/updates merge sparse fields and preserve status, kind, input/output, content and locations. Permission options are correlated through a Favetto request ID and mapped by option kind, not array position. `ask` waits with a timeout and fails closed; `allow`/`deny` answer according to configured policy. The client does not provide filesystem or terminal operations, and advertises no such capability. On `session/cancel`, pending permission requests are not immediately answered `cancelled` (see findings). |
| [Agent plan](https://agentclientprotocol.com/protocol/v1/agent-plan) | **Implemented (baseline checklist)** | `plan` updates replace the complete checklist, are normalized to `AgentStateEvent::Plan`, folded into the session snapshot, and rendered in the TUI Agent panel and GPUI Events surface. Identified/Markdown plan operations (`plan_update`/`plan_removed`) remain intentionally unadvertised and unimplemented. |
| [Elicitation](https://agentclientprotocol.com/protocol/v1/elicitation) | **Not implemented / not advertised** | The runtime handles `session/request_permission` only. It does not advertise form/URL elicitation modes or handle `elicitation/create` / `elicitation/complete`. |
| [File system](https://agentclientprotocol.com/protocol/v1/file-system) | **Not implemented / not advertised** | No `fs/read_text_file` or `fs/write_text_file` request handlers and no `fs` client capability. This correctly prevents agents from assuming the Favetto client can read editor buffers or perform client-side file operations; agents instead use their own configured tools/process environment. |
| [Terminals](https://agentclientprotocol.com/protocol/v1/terminals) | **Not implemented / not advertised** | No `terminal/*` request handlers and no `terminal` capability. Tool-call terminal references may be retained as IDs, but Favetto does not offer a client-managed terminal service. This is consistent with ACP's capability negotiation. |
| [Session config options](https://agentclientprotocol.com/protocol/v1/session-config-options) | **Implemented (v1 select/boolean subset)** | Preserves the complete setup snapshot, including grouped selects and unknown option categories/kind values, and maps `config_option_update` as a wholesale replacement. `agents.options`, `agents.set_option` and `agents.set_mode` expose the live session, with the daemon validating option ids, offered select values, booleans and mode ids before calling the agent. Task model selection still applies the requested model at setup; interactive changes are explicit user actions. |
| [Session modes](https://agentclientprotocol.com/protocol/v1/session-modes) | **Implemented** | Captures the setup `modes` snapshot, maps `current_mode_update` separately from config selectors, and switches modes through `session/set_mode` via the validated `agents.set_mode` RPC. TUI and GPUI both render the current mode and advertised choices. |
| [Slash commands](https://agentclientprotocol.com/protocol/v1/slash-commands) | **Implemented (advertised and surfaced)** | `available_commands_update` replaces the complete command list (with input hints) in both clients. Commands are ordinary prompt text: completion inserts `/name ` and the prompt is sent unchanged through `agents.prompt`, including unknown commands. |
| [Subagents](https://agentclientprotocol.com/protocol/v1/subagents) | **Not implemented / not advertised** | No `subagents` capability is advertised and `subagent_update`, session-directed messages and child-session updates are ignored. Favetto therefore cannot provide child-session attribution or the restricted child controls described by ACP. |
| [Request cancellation](https://agentclientprotocol.com/protocol/v1/cancellation) | **Partial; feature-level cancellation exists** | Favetto sends `session/cancel`, the correct v1 session-turn cancellation mechanism. Generic `$/cancel_request` is optional and not implemented. The permission-response obligation attached to session cancellation is currently missed. |
| [Errors](https://agentclientprotocol.com/protocol/v1/error) | **Partial** | The SDK handles JSON-RPC request/response correlation and errors surface as state errors; unknown stop reasons become `unknown`. Agent-reported errors are recorded. The fake server and tests cover selected error paths, not the complete ACP error-code matrix, malformed requests/messages, or all initialization/version errors. |
| [Extensibility](https://agentclientprotocol.com/protocol/v1/extensibility) | **Partial** | Unknown `SessionUpdate` variants are safely ignored. Favetto does not expose `_meta` through its normalized wire model, so custom metadata and unknown payloads are not preserved for clients/replay. No custom ACP extensions are advertised or implemented. |
| [Security](https://agentclientprotocol.com/protocol/v1/security) | **Mixed / explicit trust boundary** | Agent commands are passed as argv (not interpolated into a shell command by the ACP transport), cwd is set, stderr is not sent as protocol data, permission policy can fail closed, and TUI Markdown/control-sequence sanitization exists. ACP does not sandbox an agent: the agent process runs with daemon OS permissions, and an agent may perform operations without asking. Favetto's permission handling is advisory to the agent, not an enforcement boundary. Review output renderers and path/link handling in each UI separately. |
| [Schema](https://agentclientprotocol.com/protocol/v1/schema) | **v1 subset via SDK** | Uses `agent-client-protocol` 5.4.0's v1 schema/types rather than a Favetto-authored JSON-RPC schema. This supplies the standard v1 types and transport machinery, but does not by itself imply support for optional methods or every current v1 session-update variant. |

### Session close capability issue

ACP v1 treats `session/close` as optional and specifies the `sessionCapabilities.close` advertisement. Favetto always sends `CloseSessionRequest` after its prompt loop (`acp.rs`, around lines 712–727) without checking that capability. The code comment recognizes that not all agents advertise/answer close, and the request is bounded by a timeout, but bounded waiting does not make an unadvertised method call conformant. It should only send the request when the agent advertised close; otherwise process teardown can remain the cleanup path.

## Highest-priority correctness findings

### 1. Cancellation must settle pending permissions

ACP v1 says that when a Client sends `session/cancel`, it **MUST** answer all pending `session/request_permission` requests for that turn with the `cancelled` outcome. Favetto's cancel path only queues `AcpCommand::Cancel`, which sends the notification. Permission requests are independently waiting in `AcpResponder::decide` on a oneshot (up to `PERMISSION_TIMEOUT`, 300 seconds); `AcpCommand::Cancel` does not drain or resolve the responder's pending map. Thus an agent can remain blocked on permission after Favetto has cancelled the prompt, and the client fails the required response behavior.

There is a second cancellation concern: the connection's control task has a `cancel_pending` boolean set on the first cancel and never reset (`acp.rs`, around lines 619–629). Every later cancel command on the same interactive ACP connection is ignored. A session can accept multiple prompt turns, so a subsequent turn cannot reliably be cancelled. The single-cancel fixture/tests do not demonstrate repeated cancellation.

**Suggested fix:** track which permission requests belong to the active turn and answer them with `InputReply::Cancelled` when that turn is cancelled; reset cancellation state at the next turn boundary (or model cancellation per request/turn rather than with a connection-lifetime boolean). Add a regression test with multiple prompt/cancel cycles and a permission request outstanding at cancellation.

### 2. Usage updates have snapshot semantics, but Favetto sums them

ACP v1 `usage_update` reports the session's current context usage and cumulative cost. Favetto maps `used`/`size` correctly to context fields, but maps `cost.amount` directly to `cost_usd` without checking `cost.currency` (`acp.rs`, around lines 1261–1270). ACP explicitly allows non-USD units such as `credits`, so a credits amount can be shown as dollars.

Further, `AgentLiveState::apply` merges each `AgentStateEvent::Usage` with `AgentUsage::merge` (`agents/mod.rs`, around lines 248–252), and `merge` adds `cost_usd` (`favetto-wire/src/agent_state.rs`, around lines 104–120). If the agent emits cumulative values 0.01 then 0.02, Favetto records 0.03, not the current 0.02. Context values are correctly treated as latest snapshots, but cost is not.

**Suggested fix:** preserve the currency or only populate `cost_usd` for USD; model ACP usage updates as replacement snapshots rather than additive usage deltas (or convert them to deltas before folding). Test repeated cumulative updates and a non-USD currency.

### 3. Non-text message content is silently dropped

`map_update` handles message updates only when `content_text` returns text (`acp.rs`, around lines 1164–1191). Image, audio, resource, and other content blocks therefore produce no Favetto event. The tool-content mapper has richer support, but intentionally strips binary data and drops unrecognized variants (`acp.rs`, around lines 1322–1376). The wire `AgentStateEvent::TextDelta` only transports a string.

This is a deliberate text-first MVP, but it falls short of the ACP content model for clients that receive these updates: the ACP content page recommends that clients show a placeholder when they cannot render a block rather than silently dropping it. It also prevents Favetto from becoming an image/audio-capable client unless it advertises and supports those prompt capabilities.

**Suggested fix:** extend the event model with structured content blocks and a safe placeholder path. Continue not advertising image/audio prompt support until the UI can compose/send them and agent capabilities are checked.

## Stable v1 optional features: completeness versus safe omission

Not all omissions are protocol violations. Optional APIs are safely unavailable when Favetto does not advertise the corresponding capability. In particular, Favetto's empty client capability set means agents must not use client filesystem or terminal methods, and the no-elicitation/no-subagents/no-plan-operations capability choice prevents required UI flows from being assumed. Plain plan checklists and slash-command advertisements are baseline v1 updates and are consumed without a capability. The main loss is interoperability and UX breadth, not invalid negotiation.

The most material feature gaps for a code-agent orchestrator are:

1. **Authentication flows** — no `authenticate`, terminal-auth handoff, reconnect, or logout. Deployments need out-of-band login.
2. **Rich session interaction** — baseline plans, slash-command discovery, general config options and modes are represented, while identified/Markdown plan operations and non-text message content are not represented to clients.
3. **Elicitation and delegated work** — structured form/URL questions and subagent trees are not supported.
4. **Provider history APIs** — ACP `session/list`, `session/delete` and `session/resume` are now available through capability-gated provider-history RPCs, while `agents.history` keeps listing Favetto-created local records only.
5. **Tool services offered by the Client** — no client-side FS or terminal API is advertised, so agents cannot request editor-buffer reads/writes or Favetto-managed terminal execution over ACP. This remains coherent with the current architecture where agent-owned tools run in the subprocess.

## ACP v2 Preview comparison

The official v2 documentation labels v2 as **Preview** and recommends side-by-side v1/v2 support for implementers that migrate. Favetto is v1-only: its dependency usage is explicitly `agent_client_protocol::v1`, and it initializes using `ProtocolVersion::V1` (`acp.rs`, around lines 25–35 and 422). It neither negotiates v2 nor adapts the v2 schema. This is not a v1 bug, but it means Favetto is not compatible with a v2-only agent and does not get v2's newer lifecycle/features.

| v2 Preview change | Favetto status |
|---|---|
| v2 initialize shape: required `info`, role-neutral `capabilities`, object markers, and version negotiation | **Absent.** v1 initialize only. |
| Authentication rename to `auth/login` / `auth/logout` | **Absent.** Neither v1 nor v2 auth flows are supported. |
| Required `session/list`, `session/resume`, `session/close`; `session/load` removed | **Absent as v2.** Favetto implements v1 optional `session/load`, not v2 resume. It always makes a v1 close call without capability checking. |
| Prompt response acknowledges insertion (`messageId`); foreground state arrives as `state_update`; prompt idempotency key | **Absent.** Favetto relies on v1 `session/prompt` response containing a stop reason and treats it as turn completion. |
| Whole-message upserts, required message IDs, extensible enums and upsert/null patch semantics | **Absent.** Normalizes v1 message chunks into text deltas; no v2 message/entity store or IDs. |
| v2 tool-call upserts, content chunks, cancellable tools/terminals, agent-owned terminal display, structured diff | **Absent.** Handles v1 `tool_call` + `tool_call_update`; no v2 cancel controls or terminal lifecycle; legacy diff shape only. |
| Plans no longer gated by a plan capability | **Partially present for v1.** Plain v1 `plan` checklists are consumed and rendered; v2 identified/Markdown plan operations are absent and the `plan` capability remains unadvertised. |
| Subagents and session-directed messages are baseline client responsibilities | **Absent.** No child-session routing/representation. |
| Elicitation | **Absent.** No client handler. |
| Config option identifiers/discriminators and v2 updates | **Absent as v2.** Only v1 model select is changed through v1 RPC. |
| v2 replay on `session/resume`, `replay_complete`, current-state synchronization, and v2 idempotent/retry rules | **Absent.** Favetto has a v1 `session/load` replay gate, which is not a substitute for v2 resume semantics. |
| v2 foreground/background state and error stop reason | **Absent.** v1 idle/stop-reason mapping only; no background-work model. |
| ACP stdio transport | **Implemented for v1.** The wire framing is the same newline-delimited JSON-RPC style; v2-specific message semantics are not. |

## Testing evidence and gaps

The repository has a deterministic fake ACP server, unit tests for tool-call update merging, permission option mapping, model selection/outcome behavior, and daemon/session persistence end-to-end coverage. The fake server exercises a narrow v1 subset: initialize, new/load, set-config-option, prompt, close, plus selected title/replay/error behaviors. It is useful for deterministic Favetto tests but is not a full protocol-conformance server.

No evidence was found in this audit of running ACP's official Test Compatibility Kit against Favetto's client role or of a maintained interoperability matrix for current supported agent versions. Existing migration-spike results are historical/manual integration evidence and do not cover all present-day optional features. The current guide already calls out the provider catalog, load capability dependency, and limited restored-transcript preview; it should also mention the cancellation and usage-mapping issues above once fixed/tracked.

## Recommended order of work

1. Fix pending-permission cancellation and repeated turn cancellation; add tests.
2. Gate `session/close` on the agent's advertised close capability.
3. Correct usage snapshot/currency semantics and add tests.
4. Add structured message content/placeholder handling so non-text content is not silently lost.
5. Decide whether the target is a deliberately narrow v1 client or broader v1 feature support. If broadening, prioritize authentication, plan/config/mode/command presentation, elicitation, and subagent updates according to product needs; only advertise each capability after its full handling path exists.
6. Treat v2 as a separate compatibility project: implement negotiation and lifecycle/state semantics first, then replay and content/tool/subagent features. Do not infer v2 support from the current v1 SDK or normalized event model.

## Official sources

- [ACP introduction](https://agentclientprotocol.com/get-started/introduction)
- [ACP documentation index](https://agentclientprotocol.com/llms.txt)
- [ACP v1 overview and schema](https://agentclientprotocol.com/protocol/v1/overview)
- [ACP v1 initialization](https://agentclientprotocol.com/protocol/v1/initialization)
- [ACP v1 prompt turns](https://agentclientprotocol.com/protocol/v1/prompt-turn)
- [ACP v1 cancellation](https://agentclientprotocol.com/protocol/v1/cancellation)
- [ACP v1 tool calls](https://agentclientprotocol.com/protocol/v1/tool-calls)
- [ACP v1 session setup](https://agentclientprotocol.com/protocol/v1/session-setup)
- [ACP v1 content](https://agentclientprotocol.com/protocol/v1/content)
- [ACP v1 security](https://agentclientprotocol.com/protocol/v1/security)
- [ACP v2 migration guide](https://agentclientprotocol.com/protocol/v2/migration)
- [ACP v2 overview](https://agentclientprotocol.com/protocol/v2/overview)
- [ACP v2 protocol pages](https://agentclientprotocol.com/protocol/v2/initialization)
