# Agent state adapters

Design and implementation plan for a common, per-agent live-state interface.

Status: proposed
Scope: daemon (`crates/favetto`), shared types (`crates/favetto-core`), TUI
(`crates/favetto-tui`).
Non-goal: replacing the embedded terminal. The PTY/`vt100` pipeline stays the
source of truth for what the user sees and types; this work adds a precise,
structured *observation* channel beside it.

---

## 1. Motivation

Today favetto learns what an agent is doing in two very different ways:

- **Headless runs** (`executor::run_agent_task`): after the process exits,
  `Agent::parse_output` reads the captured raw stdout and, for opencode,
  requires *every* non-empty line to be valid JSON — one stray line discards
  the whole structured result. Only the session id is used; tool calls, text,
  token usage, and errors are stored as an opaque blob.
- **Interactive TUI sessions**: no structured view at all. Awaiting-input and
  session titles are inferred from the rendered screen (`agents/detect.rs`,
  `attention.rs`) and by polling `opencode session list`.

Meanwhile every supported CLI now exposes a machine-readable channel (HTTP/SSE,
JSONL over stdio, or hooks). This document defines the seam that lets each
agent adapter use its strongest channel while degrading to the screen heuristic
everywhere else.

### Goals

1. One normalized, wire-serializable state model shared by daemon and TUI.
2. One adapter interface; transports differ per agent, the consumer does not.
3. Precise awaiting-input/permission handling where the CLI supports it.
4. Session id, title, activity, and usage captured for interactive sessions too.
5. Structured run output for headless tasks (text, tools, usage, outcome).
6. Graceful fallback: with no channel, behaviour is exactly today's.
7. The user can always hop into the Agent tab and drive the real TUI.

---

## 2. Current seams

| Concern | Location |
|---|---|
| CLI abstraction / traits | `crates/favetto/src/agents/agent.rs` (`Agent`, `AgentRunResult`, `SessionIdProbe`) |
| PTY/session manager | `crates/favetto/src/agents/mod.rs` (`AgentManager`, `Session`, `AgentEvent`) |
| Built-ins | `agents/{opencode,claude,pi,vibe,configurable}.rs` |
| Awaiting input | `agents/detect.rs`, `attention.rs` |
| Task execution | `executor.rs` (`run_agent_task`, `build_task_output`) |
| Interactive open | `server.rs` (`start_agent`, `finish_oneshot`) |
| Shared types | `crates/favetto-core/src/model.rs`, `rpc.rs` |
| Config | `crates/favetto-core/src/config.rs` (`AgentConfig`) |

The single reader thread in `AgentManager::start` already:

- captures raw stdout into `raw` (bounded),
- runs an incremental **line-JSON probe** for the session id,
- feeds the `vt100` emulator and broadcasts frames,
- records `last_activity`.

The adapter interface extends exactly this point: a per-session parser is fed
the same bytes, and a per-session consumer applies normalized events.

---

## 3. Normalized model (`favetto-core`)

Add a new module `crates/favetto-core/src/agent_state.rs`, re-exported from
`model.rs`. All types are `Serialize`/`Deserialize`, with `#[serde(default)]`
on new fields so older clients/daemons interoperate.

```rust
/// What an agent is doing right now (coarse, for the task list / picker).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentActivity {
    Starting,
    Thinking,
    Responding,
    Tool { name: String, description: Option<String> },
    Waiting { request: InputRequest },
    Idle,
    Exited { code: Option<i32> },
}

/// A prompt the agent is blocked on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputRequest {
    /// Transport-specific correlation id (permission id, extension-ui id, …).
    pub id: String,
    pub kind: AwaitingInputKind,
    /// Human-readable prompt text.
    pub message: String,
    /// Selectable options; empty means free-form text.
    #[serde(default)]
    pub options: Vec<String>,
    /// Whether an "always" / "remember" answer is offered.
    #[serde(default)]
    pub allow_always: bool,
}

/// The answer to an `InputRequest`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum InputReply {
    Once,
    Always,
    Reject,
    Value { value: String },
    Confirmed { confirmed: bool },
    Cancelled,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentUsage {
    #[serde(default)] pub input_tokens: u64,
    #[serde(default)] pub output_tokens: u64,
    #[serde(default)] pub reasoning_tokens: u64,
    #[serde(default)] pub cache_read_tokens: u64,
    #[serde(default)] pub cache_write_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AgentStateEvent {
    Session { #[serde(default)] session_id: Option<String>,
              #[serde(default)] title: Option<String>,
              #[serde(default)] model: Option<String> },
    TurnStarted,
    TextDelta { role: MessageRole, text: String },
    ReasoningDelta { text: String },
    ToolStarted { id: String, name: String, input: serde_json::Value },
    ToolUpdated { id: String, partial: serde_json::Value },
    ToolFinished { id: String, name: String, ok: bool,
                   #[serde(default)] output: Option<serde_json::Value> },
    InputRequested { request: InputRequest },
    InputResolved { id: String },
    Usage { usage: AgentUsage },
    Title { title: String },
    Idle { outcome: IdleOutcome },
    Error { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdleOutcome { Succeeded, Failed, Interrupted }
```

### Changes to existing shared types

`AgentSessionInfo` (`favetto-core/src/model.rs`) gains two defaulted fields:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub activity: Option<AgentActivity>,
#[serde(default, skip_serializing_if = "Option::is_none")]
pub usage: Option<AgentUsage>,
```

`AwaitingInputReason` is extended (not replaced) so a remote client can answer
it through the daemon:

```rust
pub struct AwaitingInputReason {
    pub kind: AwaitingInputKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_always: bool,
}
```

`AgentCapabilities` gains (all default `false`):

```rust
/// Reports live state through a structured channel.
pub reports_state: bool,
/// Can answer permission/dialog prompts through the channel.
pub permission_channel: bool,
/// Can open an interactive session attached to an already-running headless run
/// without racing its session file (opencode's managed server).
pub concurrent_attach: bool,
```

`RunSummary` replaces the ad-hoc `AgentRunResult.output`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSummary {
    #[serde(default)] pub session_id: Option<String>,
    #[serde(default)] pub title: Option<String>,
    pub text: String,
    #[serde(default)] pub reasoning: String,
    #[serde(default)] pub tool_calls: Vec<ToolCall>,
    #[serde(default)] pub usage: AgentUsage,
    #[serde(default)] pub outcome: Option<IdleOutcome>,
    #[serde(default)] pub error: Option<String>,
}
```

`AgentRunResult` keeps its shape but `output` becomes
`serde_json::to_value(RunSummary)` for structured agents; the existing
`{"text": raw}` default is unchanged.

---

## 4. Adapter interface (`crates/favetto/src/agents/state.rs`)

### 4.1 Traits

```rust
use futures_util::future::BoxFuture;

/// A transport that produces normalized state for one agent session.
pub trait StateSource: Send + Sync {
    /// Stable label for diagnostics ("opencode-server", "pi-rpc", …).
    fn label(&self) -> &'static str;
    /// Start observing. Called once, after the child is spawned.
    fn start(&self, ctx: StateContext) -> anyhow::Result<StateStart>;
}

/// Everything a source may need about the launch it is observing.
pub struct StateContext {
    /// favetto's own session id (the `AgentManager` key).
    pub favetto_session: String,
    /// The agent's external session id, when known before launch.
    pub external_session: Option<String>,
    pub headless: bool,
    pub cwd: Option<PathBuf>,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Write access to the agent's stdin (PTY master or pipe).
    pub stdin: Arc<parking_lot::Mutex<Box<dyn Write + Send>>>,
}

pub struct StateStart {
    /// Normalized events, consumed by the session's state task.
    pub events: tokio::sync::mpsc::UnboundedReceiver<AgentStateEvent>,
    /// An incremental stdout parser; the manager feeds it every raw chunk.
    pub stdout: Option<Box<dyn StdoutParser>>,
    /// Answers prompts through this transport.
    pub responder: Arc<dyn InputResponder>,
    /// Stops a detached transport (server/SSE/hook) when dropped.
    pub stop: Option<Box<dyn FnOnce() + Send + Sync>>,
}

/// Incremental parser for a line-delimited JSON stream.
pub trait StdoutParser: Send {
    fn push(&mut self, chunk: &[u8]);
    /// End of stream; emit `Idle`/`Usage`/`Session` as appropriate.
    fn finish(&mut self, exit_code: Option<i32>);
    /// The structured summary so far, for the final task output.
    fn summary(&self) -> RunSummary;
}

pub trait InputResponder: Send + Sync {
    fn reply<'a>(
        &'a self,
        request_id: &'a str,
        reply: InputReply,
    ) -> BoxFuture<'a, anyhow::Result<()>>;
}
```

### 4.2 New `Agent` method

```rust
/// Build the live-state source for a launch, if this agent has one.
fn state_source(&self, cfg: &StateSourceConfig) -> Option<Box<dyn StateSource>> {
    None
}
```

`StateSourceConfig` is the resolved launch description
(`headless`, `command`, `args`, `env`, `cwd`) so an implementation can decide
between transports (e.g. opencode: server when a session exists, stdout JSONL
otherwise). Every existing agent keeps the default `None` until adopted.

`parse_output` becomes a thin, tolerant helper used only when no `StdoutParser`
ran; the normalized path supersedes it for structured agents.

### 4.3 Manager wiring

In `AgentManager::start`, after `spawn_command` and before the reader thread:

```text
let start = agent.state_source(&cfg).and_then(|s| s.start(ctx).ok());
// feed raw chunks to start.stdout (if any) in the reader loop
// spawn a StateTask: consume start.events -> update Session + bus + attention
```

Per-session additions to `Session`:

```rust
live: Arc<Mutex<AgentLiveState>>,   // activity + usage + awaiting_input
responder: Option<Arc<dyn InputResponder>>,
state_stop: Option<Box<dyn FnOnce() + Send + Sync>>, // dropped on close/exit
```

`AgentLiveState { activity: Option<AgentActivity>, usage: Option<AgentUsage> }`
is folded into `Session::info()`.

New method:

```rust
pub fn reply(&self, session_id: &str, request_id: &str, reply: InputReply)
    -> anyhow::Result<()>;
```

which calls the session's `InputResponder` (and is a no-op error when the
session has none).

### 4.4 Broadcasting state

Extend `AgentEvent` (high-volume, non-durable channel) with:

```rust
State { session_id: String, live: AgentLiveState },
```

The connection forwarder in `server.rs` maps it to a new
`push::AGENT_STATE = "agent.state"` notification, scoped to attached sessions
like `agent.output`. `agents.list` continues to carry the full snapshot, so a
client that misses a frame re-syncs on the next list.

Add RPC `agents.reply` (`{ session_id, request_id, reply }`), documented in the
generated remote-API reference.

### 4.5 Awaiting input

`attention::watch` gains a state branch:

```text
select! {
    ev = state_rx.recv() => match ev {
        InputRequested{request} => mark_awaiting(..., request),
        InputResolved{..} | Idle{..} => resume_task(...),
        _ => update activity/usage,
    },
    _ = poll.tick() => { /* existing screen heuristic, only if no state source */ }
}
```

Rules:

- When a session has a `StateSource`, its `InputRequested`/`InputResolved`
  events are authoritative; the screen heuristic is disabled for that session.
- Otherwise the current debounced screen heuristic runs unchanged.
- `Idle` at process exit feeds the terminal status, so the task no longer waits
  for a screen that has already scrolled.

### 4.6 Headless output

`run_agent_task` prefers `parser.summary()` (via the manager) over
`agent.parse_output(raw)`. `build_task_output` then stores:
`output.summary` = the `RunSummary`, plus the existing capped `raw`. This makes
a dependent task's `prev.output` carry real text/tools/usage/outcome instead of
a raw JSON event dump.

The headless stream is a background concern: the Agent panel never renders it. A
headless run consumes its own stdout for state/usage/session id, while the panel
opens a **concurrent interactive attach** when the agent advertises
`concurrent_attach` (opencode: the headless `run` and the interactive session
share one managed-server session). Agents without it render the structured state
view instead, so goal 7 ("the user can always hop into the Agent tab and drive
the real TUI") holds without exposing machine output.

---

## 5. Configuration

Add to `AgentConfig` (`favetto-core/src/config.rs`). All optional, defaulting to
`None` (built-in behaviour):

```toml
[agents.opencode]
# Live-state mode: "auto" (built-in default) | "none" | "stdout" | "server" | "hooks"
state = "auto"
# Structured stdout format for headless runs and final summaries.
output_format = "opencode-json"          # opencode-json | claude-stream-json |
                                         # pi-json | pi-rpc | vibe-streaming |
                                         # plain-jsonl
# opencode only: "managed" (favetto-owned serve) | "background" | "<url>"
server = "managed"

[agents.claude]
state = "auto"                            # uses stdout + hooks
hooks = true                              # inject favetto HTTP hooks via --settings
```

Rust:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStateMode { Auto, None, Stdout, Server, Hooks }

pub struct AgentConfig {
    // …existing…
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<AgentStateMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<bool>,
}
```

`capabilities_from_config` derives `structured_output` from an explicit
`output_format` (falling back to `session_id_json_key`), and `reports_state`/
`permission_channel` from `state`/known formats.

Because `AgentConfig` is schema-generated, adding fields requires regenerating
and committing `docs/reference/config.md` and
`docs/public/favetto-schema.json` (`cargo run -p favetto -- __doc`).

---

## 6. Transport taxonomy

| Transport | Fed by | Used by | Permission replies |
|---|---|---|---|
| `StdoutJsonl` | manager reader thread | all JSONL CLIs | no (unless protocol is bidirectional) |
| `StdioRpc` | reader + stdin writer | pi `--mode rpc` | yes (`extension_ui_response`) |
| `HttpSse` | detached tokio task | opencode server | yes (`POST …/permission/{id}/reply`) |
| `HookReceiver` | daemon HTTP endpoint | claude hooks | yes (`PermissionRequest` decision) |
| `FileTail` | detached tokio task | pi/claude/vibe session files | no |
| `Screen` | existing `attention` poll | fallback | via PTY keystrokes |

`StdoutJsonl` and `StdioRpc` share the `StdoutParser` half of `StateStart`;
`HttpSse` and `HookReceiver` use the detached half (`stop`).

---

## 7. Per-agent adapters

### 7.1 opencode

**Invocation.** Headless `opencode run --server <url> …`; interactive
`opencode attach <url> --session <id>`. The daemon owns a managed
`opencode serve` (random port + `OPENCODE_SERVER_PASSWORD`), or reuses the
registered background service (`~/.local/state/opencode/service.json`).

**State source (`opencode-server`).** One SSE subscription to `GET /api/event`
(`/global/event` for cross-directory), filtered by `sessionID`, reconciled
periodically against `GET /api/session/{id}` and `/message` (SSE is best-effort
by opencode's own guidance). `permission.asked` / `GET …/permission` map to
`InputRequested`; `POST …/permission/{requestID}/reply` with
`once|always|reject` maps to `InputReply`.

**Native event mapping (server SSE):**

| opencode | normalized |
|---|---|
| `session.created` / `session.updated` | `Session` / `Title` |
| `message.updated` (assistant) | `TurnStarted` (+ model) |
| `message.part.updated` `text` | `TextDelta` → `TextDelta`/`MessageCompleted` |
| `message.part.updated` `reasoning` | `ReasoningDelta` |
| `message.part.updated` `tool` running | `ToolStarted` / `ToolUpdated` |
| `message.part.updated` `tool` completed/error | `ToolFinished` |
| `message.part.updated` `step-start`/`step-finish` | `TurnStarted` / `Usage` |
| `permission.asked` | `InputRequested` |
| `session.status` `idle` | `Idle` |
| `session.error` | `Error` |

**Stdout fallback (`opencode-json`, `opencode run --format json`).** Tolerant
JSONL with types `step_start`, `text`, `reasoning`, `tool_use`, `step_finish`,
`error`; every line carries `sessionID`. Note the CLI emits only *completed*
tool states and has known truncation bugs, so this path never decides
success alone.

**Module layout.** `agents/opencode/server.rs` (lifecycle/health/auth),
`agents/opencode/observer.rs` (`StateSource`), `agents/opencode/json.rs`
(`StdoutParser`).

**Interactive.** `POST /api/session` (with `location.directory`, model, agent),
seed the prompt via `POST /api/session/{id}/prompt`, then launch
`opencode attach <url> --session <id>`. This replaces the `--prompt` + timed
Enter `submit_prompt` hack and gives interactive sessions an id, title, and
live state.

### 7.2 pi

pi is the closest analogue to opencode: a long-lived JSON protocol, but over
stdio instead of HTTP.

**Invocation.** Headless `pi --mode rpc`; one-shot alternative
`pi --mode json`. Interactive stays `pi --session-id <id>`.

**State source (`pi-rpc`).** `StdioRpc` over the PTY. Commands are JSON lines
written to the PTY stdin (`prompt`, `get_state`, `get_messages`, `abort`);
records read back are either `response` (correlated by `id`) or events.

**Native event mapping:**

| pi | normalized |
|---|---|
| `agent_start` / `agent_settled` | `TurnStarted` / `Idle` |
| `message_update.assistantMessageEvent.text_*` | `TextDelta` |
| `…thinking_*` | `ReasoningDelta` |
| `…toolcall_end` | `ToolStarted` |
| `tool_execution_start/update/end` | `ToolStarted`/`ToolUpdated`/`ToolFinished` |
| `extension_ui_request` (`select`/`confirm`/`input`/`editor`) | `InputRequested` |
| `extension_ui_response` | `InputResolved` |
| `message_update.usage` / `get_session_stats` | `Usage` |
| `session_info_changed` | `Title` |
| `agent_end` | run boundary (do not treat as final) |

**Permissions.** `extension_ui_request` → `InputRequested`; the reply is an
`extension_ui_response` on stdin (`{value}`, `{confirmed}`, or
`{cancelled:true}`), correlated by `id`.

**Session id.** `session` header (`--mode json`) or `get_state.sessionId`
(`--mode rpc`); `--session-id` already seeds it. **Title** from `--name` /
`session_info_changed`.

**Interactive.** The RPC process is headless. For the TUI, tail the session
file (`~/.pi/agent/sessions/--<cwd-slug>--/<ts>_<id>.jsonl`) for progress and
title; pending dialogs are not written until answered, so the screen fallback
remains for awaiting-input on the TUI.

**Module layout.** `agents/pi/rpc.rs`, `agents/pi/json.rs`,
`agents/pi/session_file.rs`.

### 7.3 claude

**Invocation.** Headless
`claude -p --output-format stream-json --verbose [--include-partial-messages]`.
Interactive stays bare `claude`; observability comes from hooks.

**Stdout parser (`claude-stream-json`).** NDJSON with a required `type`:

| claude | normalized |
|---|---|
| `system`/`init` | `Session` (session_id, model, `capabilities`) |
| `system`/`api_retry` | `Error` (informational) |
| `stream_event`/`content_block_delta` `text_delta` | `TextDelta` |
| `stream_event`/`content_block_delta` `thinking_delta` | `ReasoningDelta` |
| `stream_event`/`content_block_start` `tool_use` | `ToolStarted` |
| `assistant` (`text`/`thinking`/`tool_use` blocks) | completed content / `ToolStarted` |
| `user` (`tool_result` blocks) | `ToolFinished` |
| `result` | `Usage` + `Idle` (outcome from `is_error`) |

Final summary fields: `result`, `is_error`, `total_cost_usd`, `duration_ms`,
`num_turns`, `usage`, `session_id`.

**Live, including the interactive TUI (`HookReceiver`).** Inject a per-launch
`--settings <file>` that registers `type: "http"` hooks pointing at a
daemon-loopback endpoint with a per-launch token. Hook events carry
`session_id`, `cwd`, `transcript_path`, `hook_event_name`, and tool fields:

| claude hook | normalized |
|---|---|
| `SessionStart` | `Session` |
| `UserPromptSubmit` | `TurnStarted` |
| `PreToolUse` | `ToolStarted` |
| `PostToolUse` / `PostToolUseFailure` | `ToolFinished` |
| `PermissionRequest` | `InputRequested` |
| `Notification` (`permission_prompt`, `idle_prompt`, `agent_needs_input`) | `InputRequested` / attention |
| `Stop` / `StopFailure` | `Idle` (`succeeded`/`failed`) |
| `SubagentStart` / `SubagentStop` | activity detail |
| `SessionEnd` | `Idle` / `Exited` |

**Permissions.** The `PermissionRequest` hook can return
`hookSpecificOutput.permissionDecision` (`allow`/`deny`/`ask`) plus
`permissionDecisionReason`. Headless unattended runs can also use
`--permission-prompt-tool` or `--permission-prompts none`. For interactive
sessions, default to *observe-only* (no decision) so the user still sees the
dialog in the TUI.

**Never** write hooks to `~/.claude/settings.json` or project settings; always
pass an ephemeral `--settings` file. Honour any `allowedHttpHookUrls` /
`httpHookAllowedEnvVars` restrictions by falling back to screen state when the
endpoint is disallowed.

**Module layout.** `agents/claude/stream_json.rs`, `agents/claude/hooks.rs`,
daemon `agent_hooks.rs` (receiver, token check, session routing).

### 7.4 vibe

**Invocation.** Headless `vibe -p --output streaming`. Interactive stays bare
`vibe` (or `vibe --resume <id>`).

**Stdout parser (`vibe-streaming`).** Mixed envelope: `type`-tagged control
rows interleaved with OpenAI-compatible `LLMMessage` rows
(`role`/`content`/`tool_calls`/`reasoning_content`/`tool_call_id`). Skip rows
with `injected: true`.

| vibe | normalized |
|---|---|
| `{"type":"system","subtype":"init",…}` | `Session` (no session id — see below) |
| `{"role":"assistant","content":…}` | `TextDelta` |
| `{"role":"assistant","reasoning_content":…}` | `ReasoningDelta` |
| `{"role":"assistant","tool_calls":[…]}` | `ToolStarted` |
| `{"role":"tool","tool_call_id":…,"content":…}` | `ToolFinished` |
| `{"type":"result","usage":…,"total_cost_usd":…}` | `Usage` + `Idle` |

**Session id.** The streaming output does *not* include `session_id` (upstream
issue #208). Discover it from `~/.vibe/logs/session/.session_index.json` /
`session_*/meta.json` (keyed by cwd + start time), or leave it blank. Title
comes from the same index.

**Permissions.** None (headless uses `--auto-approve`). Interactive state stays
screen-based.

**Module layout.** `agents/vibe/stream.rs`, `agents/vibe/session_index.rs`.

### 7.5 configurable / custom

- `output_format = "plain-jsonl"` enables a permissive parser: for each JSON
  object, emit `TextDelta` for a top-level string `text`/`content`, and
  `ToolStarted`/`ToolFinished` for `{type:"tool_use"}`-shaped rows. Everything
  else is preserved as opaque `RunSummary.tool_calls` entries.
- `session_id_json_key` keeps working as today.
- No live transport; screen fallback.

---

## 8. Format references

- opencode server API: `GET /openapi.json` on a running server; `GET /api/event`
  (SSE); `/api/session`, `/api/session/{id}/message`,
  `/api/session/{id}/permission`. JSONL: opencode `run --format json`.
- pi JSON event stream: <https://pi.dev/docs/latest/json>
- pi RPC: <https://pi.dev/docs/latest/rpc> and
  <https://pi.dev/docs/latest/rpc-extension-ui>
- claude headless / stream-json:
  <https://code.claude.com/docs/en/headless> and the Agent SDK streaming page.
- claude hooks: <https://code.claude.com/docs/en/hooks>
- vibe `--output streaming`: OpenAI-compatible `LLMMessage` rows; upstream
  issue mistralai/mistral-vibe#208 for the missing session id.

Prefer feature-detection over version sniffing: claude's `system/init` carries a
`capabilities` array; opencode/pi expose version endpoints. Session-file
formats are private and may change; protocol streams are the stable path.

---

## 9. Delivery phases

Each phase is independently shippable and leaves fallback intact.

1. **Common model + tolerant parsing.**
   - Add `agent_state.rs` types and defaulted fields; add `RunSummary`.
   - Replace `OpenCodeAgent::parse_output`'s all-or-nothing behaviour with a
     tolerant `StdoutParser` (ignore non-JSON lines, keep the session id).
   - No transports yet; headless output improves for opencode immediately.
2. **Interface + screen fallback preserved.**
   - Add `StateSource`/`StdoutParser`/`InputResponder`, `StateSourceConfig`,
     `Agent::state_source`, per-session `AgentLiveState`, `AgentEvent::State`,
     `push::AGENT_STATE`, `agents.reply`, and `AgentManager::reply`.
   - `attention::watch` consumes state when present, else screens.
3. **pi (`pi-rpc`).** Lowest-effort, highest-fidelity second target; validates
   the interface including prompt replies.
4. **claude (`claude-stream-json` + hooks).** Adds the `HookReceiver` transport
   and per-launch `--settings` injection.
5. **vibe (`vibe-streaming` + session index).** Output-only; screen fallback for
   interactive.
6. **opencode (`opencode-server`).** Managed `serve`, session creation,
   `attach --session`, SSE observer; replaces the prompt-submit hack.
7. **TUI.** Show `activity`/`usage` in the task list/picker, render the precise
   permission prompt, and add a keybind to `agents.reply`.
8. **Configurable/custom `plain-jsonl`** and docs.

Suggested commit split (Conventional Commits, per `AGENT.md`):
`feat(agents): add normalized agent state types`,
`feat(agents): add state source adapter interface`,
`feat(agents): observe pi over rpc`, `feat(agents): observe claude via
stream-json and hooks`, `feat(agents): observe vibe streaming output`,
`feat(agents): manage an opencode server for state and sessions`
(a `feat!` if any wire field becomes required).

---

## 10. Testing

- **Unit**: each `StdoutParser` gets golden JSONL fixtures (happy path, noise,
  truncated stream, error rows) asserting the exact `AgentStateEvent` sequence
  and `RunSummary`. Include the known opencode `step_start`-only truncation and
  vibe's `injected:true` rows.
- **Fake transports**: a `StateSource` test double drives `attention::watch`
  through `InputRequested → InputResolved` and asserts task status/events.
- **RPC round-trip**: `AgentManager::start` + `reply` against a scripted
  `sh`/`cat` fixture (no real CLIs), like the existing PTY tests.
- **Golden summaries**: assert `build_task_output` embeds `RunSummary`, not the
  raw event array.
- **Compat**: deserialize legacy `AgentSessionInfo`/`AwaitingInputReason`
  payloads (missing new fields) unchanged.
- **Live smoke (ignored by default)**: a `#[ignore]` test gated on an installed
  CLI + auth, one per agent, asserting a minimal prompt yields a session id and
  an `Idle` event.
- Existing gates from `AGENT.md`: `cargo fmt`, `clippy -D warnings`,
  `nextest`, `__doc` diff, and `docs:check`.

---

## 11. Risks and mitigations

| Risk | Mitigation |
|---|---|
| Only opencode/pi/claude have a permission channel | Model `permission_channel` as a capability; vibe/custom keep the screen heuristic and are documented as such |
| Mutating user agent config | claude hooks via ephemeral `--settings`; never touch global/project settings |
| Daemon hook endpoint exposure | loopback bind + per-launch bearer token; disabled unless `hooks = true` |
| opencode SSE gaps / `run --format json` truncation | SSE is a hint; reconcile via REST; never decide success from stdout alone |
| Version/format drift in session files | Prefer protocol streams; feature-detect (`capabilities`); keep file tail best-effort |
| Duplicate writers on a session | Observers are read-only except explicit prompt replies; interactive sessions are driven only through their own TUI/PTY |
| Larger `AgentSessionInfo` payloads | `activity`/`usage` are small; `agents.list` snapshots plus scoped `agent.state` pushes keep frames bounded |
| Server lifecycle failure | supervised `opencode serve` with backoff; on failure fall back to stdout JSONL + screen, session unaffected |

---

## 12. Resolved decisions

The questions left open above were decided when the work was split into issues;
the answers are recorded here and mirrored in the tracking issue
(`oknozor/favetto#140`).

1. **One managed opencode server per daemon**, matching the background-service
   model and avoiding N servers. It is supervised with backoff; if it fails,
   observation degrades to stdout JSONL + screen without affecting the session.
   (Issue #159.)
2. **`agents.reply` is a dedicated method**, not an extension of `agents.input`.
   Replies are typed (`InputReply`) and correlated by `request_id`, so a
   dedicated, validatable method is simpler and introspectable; `agents.input`
   stays the raw-byte PTY channel. (Issue #155.)
3. **Only the folded snapshot travels on the wire.** `AgentSessionInfo` carries
   `activity`/`usage` and a scoped `agent.state` push covers changes; raw
   `AgentStateEvent`s stay internal to keep frame volume bounded, and clients
   resync from `agents.list`. (Issue #155.)
4. **claude interactive sessions default to observe-only.** The hook receiver
   reports `PermissionRequest` but returns no decision for interactive sessions,
   so the user still answers in the real TUI; headless runs may decide via
   `--permission-prompt-tool` or config. (Issue #157.)
