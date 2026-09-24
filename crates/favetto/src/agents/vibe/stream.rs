//! Tolerant parser and state source for `vibe --output streaming`.
//!
//! The design doc (`docs/design/agent-state-adapters.md` §7.4) describes this
//! stream as OpenAI-compatible `LLMMessage` rows plus a `{"type":"result"}` row.
//! That description is stale: the installed Vibe emits one JSON *history entry*
//! per line, `type`-tagged (`message`, `reasoning`, `effect`), with `sessionId`
//! on every row and **no** usage row. This parser treats the grounded entry
//! format as the primary path and keeps the design's OpenAI/`result` rows as
//! tolerant aliases, then backfills the session id, title and usage from the
//! on-disk session store (see [`super::session_index`]).
//!
//! Like every other stdout parser, it ignores non-JSON noise and a single
//! malformed line never discards the run. The reader thread feeds raw PTY bytes
//! through [`StdoutParser::push`]; [`StdoutParser::finish`] folds the stream
//! into a [`RunSummary`] and emits the terminal `Idle`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use futures_util::future::BoxFuture;
use tokio::sync::mpsc;

use favetto_core::model::{
    AgentStateEvent, AgentUsage, IdleOutcome, InputReply, MessageRole, RunSummary, ToolCall,
};

use crate::agents::state::{
    InputResponder, StateContext, StateSource, StateSourceConfig, StateStart, StdoutParser,
};

use super::session_index;

/// A streaming, noise-tolerant `vibe-streaming` parser.
pub(crate) struct VibeStreamParser {
    /// Bytes seen but not yet terminated by a newline.
    buffer: Vec<u8>,
    /// Every event observed, in order (kept for tests and `parse_output`).
    events: Vec<AgentStateEvent>,
    /// When present, every event is forwarded here as it is observed.
    tx: Option<mpsc::UnboundedSender<AgentStateEvent>>,
    summary: RunSummary,
    /// The Vibe home, resolution root for the session store (live path only).
    root: Option<PathBuf>,
    /// The working directory of the run (live path only).
    cwd: Option<PathBuf>,
    /// When the run started, used to disambiguate the session by start time.
    since: Option<DateTime<Utc>>,
    /// The last `turnId` seen, so `TurnStarted` is emitted once per turn.
    last_turn_id: Option<String>,
    /// tool_call id → tool name, to label a later `role:"tool"` result.
    tool_names: HashMap<String, String>,
    /// At least one recognized row was seen (the stream is machine-readable).
    saw_structured: bool,
    /// An error row was seen.
    saw_error: bool,
    /// Assistant content or a tool was observed (a turn produced work).
    saw_completion: bool,
    finished: bool,
}

impl VibeStreamParser {
    /// A parser that only accumulates events and the summary (`parse_output`).
    pub(crate) fn new() -> Self {
        Self {
            buffer: Vec::new(),
            events: Vec::new(),
            tx: None,
            summary: RunSummary::default(),
            root: None,
            cwd: None,
            since: None,
            last_turn_id: None,
            tool_names: HashMap::new(),
            saw_structured: false,
            saw_error: false,
            saw_completion: false,
            finished: false,
        }
    }

    /// A parser that also forwards events to `tx` (the live state channel).
    pub(crate) fn with_channel(tx: mpsc::UnboundedSender<AgentStateEvent>) -> Self {
        Self {
            tx: Some(tx),
            ..Self::new()
        }
    }

    /// Attach the session store so `finish` can resolve a missing session id,
    /// title, and usage.
    pub(crate) fn with_index(mut self, root: PathBuf, cwd: PathBuf, since: DateTime<Utc>) -> Self {
        self.root = Some(root);
        self.cwd = Some(cwd);
        self.since = Some(since);
        self
    }

    /// The structured result built so far.
    pub(crate) fn summary(&self) -> RunSummary {
        self.summary.clone()
    }

    /// The normalized events observed so far, in order.
    #[cfg(test)]
    pub(crate) fn events(&self) -> &[AgentStateEvent] {
        &self.events
    }

    /// Record one event: forward it to the channel (when present) and keep it.
    fn emit(&mut self, event: AgentStateEvent) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(event.clone());
        }
        self.events.push(event);
    }

    /// Feed a raw output chunk. Complete lines are parsed immediately; a partial
    /// trailing line is held until more bytes (or [`Self::finish`]) arrive.
    pub(crate) fn push(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
        while let Some(pos) = self.buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            self.process_line(line.trim_end_matches(['\r', '\n']));
        }
    }

    /// Flush any trailing partial line and record the run's terminal state.
    ///
    /// `exit_code` is a tie-breaker, never a claim of success for a stream that
    /// produced no structured rows.
    pub(crate) fn finish(&mut self, exit_code: Option<i32>) {
        if self.finished {
            return;
        }
        self.finished = true;
        let tail = std::mem::take(&mut self.buffer);
        let tail = String::from_utf8_lossy(&tail);
        let tail = tail.trim();
        if !tail.is_empty() {
            self.process_line(tail);
        }
        if !self.saw_structured {
            // No machine-readable rows: leave `outcome` unset, exactly as an
            // unstructured run did before.
            return;
        }
        self.backfill_from_index();
        if self.summary.outcome.is_none() {
            let outcome = if self.saw_error || matches!(exit_code, Some(code) if code != 0) {
                IdleOutcome::Failed
            } else if self.saw_completion {
                IdleOutcome::Succeeded
            } else {
                // Structured rows but nothing that proves success: do not report
                // a truncated observation as a clean run.
                IdleOutcome::Interrupted
            };
            self.summary.outcome = Some(outcome);
            self.emit(AgentStateEvent::Idle { outcome });
        }
    }

    /// Resolve a missing session id/title/usage from Vibe's on-disk store.
    ///
    /// Best-effort: any missing file or malformed record leaves the summary
    /// untouched. Only the live path attaches an index.
    fn backfill_from_index(&mut self) {
        let (Some(root), Some(cwd)) = (self.root.clone(), self.cwd.clone()) else {
            return;
        };
        match self.summary.session_id.clone() {
            None => {
                let Some(since) = self.since else {
                    return;
                };
                if let Some(identity) = session_index::find_recent_session(&root, &cwd, since) {
                    self.summary.session_id = Some(identity.id.clone());
                    self.summary.title = identity.title.clone();
                    self.emit(AgentStateEvent::Session {
                        session_id: Some(identity.id),
                        title: identity.title,
                        model: None,
                    });
                }
            }
            Some(session_id) => {
                if self.summary.title.is_none() {
                    if let Some(identity) = session_index::find_session_in(&root, &session_id) {
                        if let Some(title) = identity.title {
                            self.summary.title = Some(title.clone());
                            self.emit(AgentStateEvent::Title { title });
                        }
                    }
                }
                if self.summary.usage.is_empty() {
                    if let Some(usage) = session_index::read_meta_usage(&root, &session_id) {
                        self.summary.usage.merge(&usage);
                        self.emit(AgentStateEvent::Usage { usage });
                    }
                }
            }
        }
    }

    fn process_line(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            // Noise (a PTY echo, warning, or truncated write): ignore it.
            return;
        };
        if value.as_object().is_none() {
            return;
        }
        // Defensive §7.4 skip: rows persisted/echoed with `injected: true` are
        // replay artifacts, not live agent output.
        if value.get("injected").and_then(|v| v.as_bool()) == Some(true) {
            return;
        }

        // A new `turnId` starts a turn; emit the boundary once per turn.
        if let Some(turn_id) = value.get("turnId").and_then(|v| v.as_str()) {
            if self.last_turn_id.as_deref() != Some(turn_id) {
                self.last_turn_id = Some(turn_id.to_string());
                self.emit(AgentStateEvent::TurnStarted);
            }
        }

        match value
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
        {
            "message" => self.process_message(&value),
            "reasoning" => self.process_reasoning(&value),
            "effect" => self.process_effect(&value),
            // Design-doc aliases (the stream the section was written against).
            "text" => {
                let text = json_str(&value, "/text");
                self.record_text(text);
            }
            "tool_use" => self.process_tool_use(&value),
            "step_start" => {
                self.saw_structured = true;
                self.emit(AgentStateEvent::TurnStarted);
            }
            "step_finish" => self.process_step_finish(&value),
            "error" => {
                let message = json_str(&value, "/error/message");
                let message = if message.is_empty() {
                    json_str(&value, "/message")
                } else {
                    message
                };
                let message = if message.is_empty() {
                    "agent error"
                } else {
                    message
                };
                self.record_error(message.to_string());
            }
            "system" => self.process_system(&value),
            "result" => self.process_result(&value),
            // A design-doc `LLMMessage` row carries a `role` and no control
            // `type`; everything else is unrelated.
            _ => {
                if value.get("role").is_some() {
                    self.process_message(&value);
                }
            }
        }

        // Every grounded row carries the session id; announce it once.
        let session_id = json_str(&value, "/sessionId").trim().to_string();
        if self.summary.session_id.is_none() && !session_id.is_empty() {
            self.summary.session_id = Some(session_id.clone());
            self.emit(AgentStateEvent::Session {
                session_id: Some(session_id),
                title: None,
                model: None,
            });
        }
    }

    /// `{"type":"message","role":"assistant"|"tool"|"user", …}` entry rows.
    fn process_message(&mut self, value: &serde_json::Value) {
        match json_str(value, "/role") {
            "assistant" => {
                let text = content_text(value.get("content"));
                self.record_text(&text);
                let reasoning = json_str(value, "/reasoning_content");
                if !reasoning.is_empty() {
                    self.saw_structured = true;
                    self.summary.reasoning.push_str(reasoning);
                    self.emit(AgentStateEvent::ReasoningDelta {
                        text: reasoning.to_string(),
                    });
                }
                if let Some(calls) = value.get("tool_calls").and_then(|v| v.as_array()) {
                    for call in calls {
                        self.start_tool_call(call);
                    }
                }
            }
            "tool" => {
                let id = json_str(value, "/tool_call_id").to_string();
                let name = self
                    .tool_names
                    .get(&id)
                    .cloned()
                    .or_else(|| {
                        let name = json_str(value, "/name");
                        (!name.is_empty()).then(|| name.to_string())
                    })
                    .unwrap_or_default();
                let ok = value.get("is_error").and_then(|v| v.as_bool()) != Some(true)
                    && value.get("error").is_none();
                let output = value.get("content").cloned();
                self.finish_tool_call(id, name, ok, output);
            }
            _ => {}
        }
    }

    /// `{"type":"reasoning","text":…}` grounded rows.
    fn process_reasoning(&mut self, value: &serde_json::Value) {
        let text = json_str(value, "/text");
        if !text.is_empty() {
            self.saw_structured = true;
            self.summary.reasoning.push_str(text);
            self.emit(AgentStateEvent::ReasoningDelta {
                text: text.to_string(),
            });
        }
    }

    /// `{"type":"effect", …}` grounded tool-lifecycle rows: one row carries the
    /// tool call and (on completion) its result.
    fn process_effect(&mut self, value: &serde_json::Value) {
        self.saw_structured = true;
        let id = json_str(value, "/id").to_string();
        let name = {
            let name = json_str(value, "/detail/toolName");
            if name.is_empty() {
                json_str(value, "/title").to_string()
            } else {
                name.to_string()
            }
        };
        let input = value
            .pointer("/detail/input")
            .cloned()
            .or_else(|| value.pointer("/detail/display").cloned())
            .unwrap_or(serde_json::Value::Null);
        let status = json_str(value, "/state/status");
        let terminal = matches!(
            status,
            "completed" | "success" | "succeeded" | "failed" | "error"
        );
        let ok = value
            .pointer("/state/display/success")
            .and_then(|v| v.as_bool())
            .unwrap_or(matches!(status, "completed" | "success" | "succeeded"));
        let output = match value.pointer("/state/outputText").and_then(|v| v.as_str()) {
            Some(text) if !text.is_empty() => Some(serde_json::Value::String(text.to_string())),
            _ => value.pointer("/state/output").cloned(),
        };

        self.tool_names.insert(id.clone(), name.clone());
        self.emit(AgentStateEvent::ToolStarted {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
        });
        if terminal {
            self.saw_completion = true;
            self.finish_tool_call(id, name, ok, output);
        }
    }

    /// A design-doc `{"type":"tool_use","id":…,"name":…,"input":…}` alias.
    fn process_tool_use(&mut self, value: &serde_json::Value) {
        self.saw_structured = true;
        self.saw_completion = true;
        let id = json_str(value, "/id").to_string();
        let name = json_str(value, "/name").to_string();
        let input = value
            .get("input")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        self.tool_names.insert(id.clone(), name.clone());
        self.emit(AgentStateEvent::ToolStarted { id, name, input });
    }

    /// A design-doc `{"type":"step_finish","part":{tokens,cost}}` alias.
    fn process_step_finish(&mut self, value: &serde_json::Value) {
        let usage = AgentUsage {
            input_tokens: u64_first(value, &["/part/tokens/input", "/part/tokens/prompt"]),
            output_tokens: u64_first(value, &["/part/tokens/output", "/part/tokens/completion"]),
            reasoning_tokens: u64_first(value, &["/part/tokens/reasoning"]),
            cache_read_tokens: u64_first(
                value,
                &["/part/tokens/cache/read", "/part/tokens/cached"],
            ),
            cache_write_tokens: u64_first(value, &["/part/tokens/cache/write"]),
            cost_usd: value
                .pointer("/part/cost")
                .and_then(|v| v.as_f64())
                .or_else(|| value.get("cost").and_then(|v| v.as_f64())),
        };
        if usage.is_empty() {
            return;
        }
        self.saw_structured = true;
        self.summary.usage.merge(&usage);
        self.emit(AgentStateEvent::Usage { usage });
    }

    /// A legacy `{"type":"system","subtype":"init",…}` header.
    fn process_system(&mut self, value: &serde_json::Value) {
        if json_str(value, "/subtype") != "init" {
            return;
        }
        self.saw_structured = true;
        let model = {
            let model = json_str(value, "/model");
            if model.is_empty() {
                let other = json_str(value, "/model_name");
                (!other.is_empty()).then(|| other.to_string())
            } else {
                Some(model.to_string())
            }
        };
        let session_id = {
            let id = json_str(value, "/sessionId");
            let id = if id.is_empty() {
                json_str(value, "/session_id")
            } else {
                id
            };
            (!id.is_empty()).then(|| id.to_string())
        };
        if self.summary.session_id.is_none() {
            self.summary.session_id = session_id.clone();
        }
        self.emit(AgentStateEvent::Session {
            session_id,
            title: None,
            model,
        });
    }

    /// A design-doc `{"type":"result","usage":…}` terminal row.
    fn process_result(&mut self, value: &serde_json::Value) {
        self.saw_structured = true;
        self.saw_completion = true;
        let usage = AgentUsage {
            input_tokens: u64_first(value, &["/usage/input_tokens", "/usage/prompt_tokens"]),
            output_tokens: u64_first(value, &["/usage/output_tokens", "/usage/completion_tokens"]),
            reasoning_tokens: u64_first(value, &["/usage/reasoning_tokens"]),
            cache_read_tokens: u64_first(
                value,
                &[
                    "/usage/cache_read_tokens",
                    "/usage/cache_read_input_tokens",
                    "/usage/cached_tokens",
                ],
            ),
            cache_write_tokens: u64_first(
                value,
                &[
                    "/usage/cache_write_tokens",
                    "/usage/cache_creation_input_tokens",
                ],
            ),
            cost_usd: value
                .get("total_cost_usd")
                .and_then(|v| v.as_f64())
                .or_else(|| value.get("cost_usd").and_then(|v| v.as_f64()))
                .or_else(|| value.pointer("/usage/cost_usd").and_then(|v| v.as_f64())),
        };
        self.summary.usage.merge(&usage);
        self.emit(AgentStateEvent::Usage { usage });

        let is_error = value.get("is_error").and_then(|v| v.as_bool()) == Some(true);
        if is_error {
            self.saw_error = true;
            let message = {
                let message = json_str(value, "/error");
                if message.is_empty() {
                    json_str(value, "/subtype").to_string()
                } else {
                    message.to_string()
                }
            };
            if !message.is_empty() {
                self.summary.error = Some(message.clone());
                self.emit(AgentStateEvent::Error { message });
            }
        }
        let outcome = if is_error {
            IdleOutcome::Failed
        } else {
            IdleOutcome::Succeeded
        };
        self.summary.outcome = Some(outcome);
        self.emit(AgentStateEvent::Idle { outcome });
    }

    /// Append a text run to the summary and emit a `TextDelta` when non-empty.
    fn record_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.saw_structured = true;
        self.saw_completion = true;
        self.summary.text.push_str(text);
        self.emit(AgentStateEvent::TextDelta {
            role: MessageRole::Assistant,
            text: text.to_string(),
        });
    }

    /// Start a design-doc assistant `tool_calls[]` entry.
    fn start_tool_call(&mut self, call: &serde_json::Value) {
        self.saw_structured = true;
        self.saw_completion = true;
        let id = json_str(call, "/id").to_string();
        let name = {
            let name = json_str(call, "/function/name");
            if name.is_empty() {
                json_str(call, "/name").to_string()
            } else {
                name.to_string()
            }
        };
        let input = call
            .pointer("/function/arguments")
            .map(parse_arguments)
            .or_else(|| call.get("input").cloned())
            .unwrap_or(serde_json::Value::Null);
        self.tool_names.insert(id.clone(), name.clone());
        self.emit(AgentStateEvent::ToolStarted { id, name, input });
    }

    /// Emit a `ToolFinished` and record the tool call in the summary.
    fn finish_tool_call(
        &mut self,
        id: String,
        name: String,
        ok: bool,
        output: Option<serde_json::Value>,
    ) {
        self.saw_structured = true;
        self.emit(AgentStateEvent::ToolFinished {
            id: id.clone(),
            name: name.clone(),
            ok,
            output: output.clone(),
        });
        self.summary.tool_calls.push(ToolCall {
            id,
            name,
            input: serde_json::Value::Null,
            ok,
            output,
        });
    }

    fn record_error(&mut self, message: String) {
        self.saw_structured = true;
        self.saw_error = true;
        self.summary.error = Some(message.clone());
        self.emit(AgentStateEvent::Error { message });
    }
}

impl StdoutParser for VibeStreamParser {
    fn push(&mut self, chunk: &[u8]) {
        VibeStreamParser::push(self, chunk);
    }

    fn finish(&mut self, exit_code: Option<i32>) {
        VibeStreamParser::finish(self, exit_code);
    }

    fn summary(&self) -> RunSummary {
        self.summary.clone()
    }
}

/// The `vibe-streaming` state source: a stdout parser fed by the manager.
pub(crate) struct VibeStreamSource {
    cwd: Option<PathBuf>,
}

impl StateSource for VibeStreamSource {
    fn label(&self) -> &'static str {
        "vibe-streaming"
    }

    fn start(&self, _ctx: StateContext) -> anyhow::Result<StateStart> {
        let (tx, events) = mpsc::unbounded_channel();
        let mut parser = VibeStreamParser::with_channel(tx);
        if let (Some(root), Some(cwd)) = (session_index::home(), self.cwd.clone()) {
            parser = parser.with_index(root, cwd, Utc::now());
        }
        Ok(StateStart {
            events,
            stdout: Some(Box::new(parser)),
            responder: Arc::new(NoopResponder),
            stop: None,
        })
    }
}

/// Vibe has no structured permission channel: replies are never accepted.
struct NoopResponder;

impl InputResponder for NoopResponder {
    fn reply<'a>(
        &'a self,
        _request_id: &'a str,
        _reply: InputReply,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async { Err(anyhow::anyhow!("vibe has no permission channel")) })
    }
}

/// Build the headless state source for a Vibe launch.
///
/// Only a headless launch that actually resolved to `--output streaming` opts
/// in; interactive Vibe (or a user override) keeps the debounced screen
/// heuristic.
pub(crate) fn state_source(cfg: &StateSourceConfig) -> Option<Box<dyn StateSource>> {
    let streaming = cfg.headless
        && cfg
            .args
            .windows(2)
            .any(|pair| pair[0] == "--output" && pair[1] == "streaming");
    streaming.then(|| {
        Box::new(VibeStreamSource {
            cwd: cfg.cwd.clone(),
        }) as Box<dyn StateSource>
    })
}

/// Join an entry's `content` into plain text: an array of `{"text": …}` parts,
/// or a bare string.
fn content_text(content: Option<&serde_json::Value>) -> String {
    match content {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(|v| v.as_str()))
            .collect(),
        _ => String::new(),
    }
}

/// Parse an OpenAI tool-call `arguments` field: a JSON string (the common case)
/// or an already-decoded object.
fn parse_arguments(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => {
            serde_json::from_str(text).unwrap_or_else(|_| serde_json::Value::String(text.clone()))
        }
        other => other.clone(),
    }
}

/// Read a top-level string field, returning `""` when missing.
fn json_str<'a>(value: &'a serde_json::Value, pointer: &str) -> &'a str {
    value
        .pointer(pointer)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
}

/// Read the first present `u64` from a list of JSON pointers, defaulting to `0`.
fn u64_first(value: &serde_json::Value, pointers: &[&str]) -> u64 {
    pointers
        .iter()
        .find_map(|pointer| value.pointer(pointer).and_then(|v| v.as_u64()))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str, exit_code: Option<i32>) -> VibeStreamParser {
        let mut parser = VibeStreamParser::new();
        parser.push(raw.as_bytes());
        parser.finish(exit_code);
        parser
    }

    /// Verbatim rows from a `vibe 2.25.7 --output streaming` run, trimmed.
    const GROUNDED: &str = concat!(
        r#"{"id":"user-turn-1-2","sessionId":"09533137-2f6b","turnId":"turn-1","type":"message","role":"user","content":[{"type":"text","text":"Run echo hi"}],"source":"turn_start"}"#,
        "\n",
        r#"{"id":"reasoning-1-0","sessionId":"09533137-2f6b","turnId":"turn-1","type":"reasoning","text":"I should run the command.","summary":[]}"#,
        "\n",
        r#"{"id":"effect-1","sessionId":"09533137-2f6b","turnId":"turn-1","type":"effect","title":"file_system.bash","detail":{"toolName":"file_system.bash","kind":"shell","input":{"command":"echo hello-from-vibe"},"display":{"summary":"bash: echo hello-from-vibe"}},"state":{"status":"completed","output":{"stdout":"hello-from-vibe\n","stderr":"","output":"","truncated":false},"outputText":"hello-from-vibe\n","durationMs":0.0,"display":{"success":true,"verb":"Ran","message":"echo hello-from-vibe","warnings":[]},"decision":"execute","approvalType":"always"}}"#,
        "\n",
        r#"{"id":"assistant-1","sessionId":"09533137-2f6b","turnId":"turn-1","type":"message","role":"assistant","content":[{"type":"text","text":"hello-from-vibe"}],"source":"harness"}"#,
        "\n",
    );

    #[test]
    fn grounded_happy_path_maps_text_reasoning_tools_and_session() {
        let parser = parse(GROUNDED, Some(0));
        assert_eq!(
            parser.events(),
            &[
                AgentStateEvent::TurnStarted,
                AgentStateEvent::Session {
                    session_id: Some("09533137-2f6b".to_string()),
                    title: None,
                    model: None,
                },
                AgentStateEvent::ReasoningDelta {
                    text: "I should run the command.".to_string(),
                },
                AgentStateEvent::ToolStarted {
                    id: "effect-1".to_string(),
                    name: "file_system.bash".to_string(),
                    input: serde_json::json!({ "command": "echo hello-from-vibe" }),
                },
                AgentStateEvent::ToolFinished {
                    id: "effect-1".to_string(),
                    name: "file_system.bash".to_string(),
                    ok: true,
                    output: Some(serde_json::json!("hello-from-vibe\n")),
                },
                AgentStateEvent::TextDelta {
                    role: MessageRole::Assistant,
                    text: "hello-from-vibe".to_string(),
                },
                AgentStateEvent::Idle {
                    outcome: IdleOutcome::Succeeded,
                },
            ]
        );

        let summary = parser.summary();
        assert_eq!(summary.session_id.as_deref(), Some("09533137-2f6b"));
        assert_eq!(summary.text, "hello-from-vibe");
        assert_eq!(summary.reasoning, "I should run the command.");
        assert_eq!(summary.tool_calls.len(), 1);
        assert_eq!(summary.tool_calls[0].id, "effect-1");
        assert_eq!(summary.tool_calls[0].name, "file_system.bash");
        assert!(summary.tool_calls[0].ok);
        assert_eq!(summary.outcome, Some(IdleOutcome::Succeeded));
    }

    #[test]
    fn design_result_row_adds_usage_and_outcome() {
        let raw = format!(
            "{}{}",
            GROUNDED,
            concat!(
                r#"{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.01,"usage":{"prompt_tokens":100,"completion_tokens":20,"reasoning_tokens":5,"cached_tokens":3}}"#,
                "\n"
            )
        );
        let parser = parse(&raw, Some(0));
        let last = parser.events().last().unwrap();
        assert_eq!(
            last,
            &AgentStateEvent::Idle {
                outcome: IdleOutcome::Succeeded
            }
        );
        let usage = &parser.summary().usage;
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 20);
        assert_eq!(usage.reasoning_tokens, 5);
        assert_eq!(usage.cache_read_tokens, 3);
        assert_eq!(usage.cost_usd, Some(0.01));
        // Exactly one Idle: `result` already set the outcome.
        let idles = parser
            .events()
            .iter()
            .filter(|e| matches!(e, AgentStateEvent::Idle { .. }))
            .count();
        assert_eq!(idles, 1);
    }

    #[test]
    fn grounded_session_id_is_read_from_rows() {
        let parser = parse(GROUNDED, Some(0));
        assert_eq!(
            parser.summary().session_id.as_deref(),
            Some("09533137-2f6b")
        );
        assert!(parser.events().iter().any(|e| matches!(
            e,
            AgentStateEvent::Session { session_id: Some(id), .. } if id == "09533137-2f6b"
        )));
    }

    #[test]
    fn effect_row_maps_to_a_tool_call() {
        let parser = parse(GROUNDED, Some(0));
        let finished = parser
            .events()
            .iter()
            .find_map(|event| match event {
                AgentStateEvent::ToolFinished {
                    id,
                    name,
                    ok,
                    output,
                } => Some((id.clone(), name.clone(), *ok, output.clone())),
                _ => None,
            })
            .unwrap();
        assert_eq!(finished.0, "effect-1");
        assert_eq!(finished.1, "file_system.bash");
        assert!(finished.2);
        assert_eq!(finished.3, Some(serde_json::json!("hello-from-vibe\n")));
    }

    #[test]
    fn effect_failure_marks_the_tool_failed() {
        let raw = concat!(
            r#"{"id":"effect-2","sessionId":"s","turnId":"t","type":"effect","title":"file_system.bash","detail":{"toolName":"file_system.bash","input":{"command":"false"}},"state":{"status":"failed","outputText":"boom","display":{"success":false}}}"#,
            "\n",
        );
        let parser = parse(raw, Some(0));
        let summary = parser.summary();
        // A failed tool is still structured work, but the run is not `Failed`
        // just because a tool failed (no error row): outcome stays succeeded.
        assert_eq!(summary.tool_calls.len(), 1);
        assert!(!summary.tool_calls[0].ok);
        assert_eq!(summary.outcome, Some(IdleOutcome::Succeeded));
    }

    #[test]
    fn injected_rows_are_skipped() {
        let raw = concat!(
            r#"{"id":"x","sessionId":"s","turnId":"t","type":"message","role":"assistant","injected":true,"content":[{"type":"text","text":"leaked"}]}"#,
            "\n",
            r#"{"id":"y","sessionId":"s","turnId":"t","type":"message","role":"assistant","content":[{"type":"text","text":"real"}]}"#,
            "\n",
        );
        let parser = parse(raw, Some(0));
        assert_eq!(parser.summary().text, "real");
        assert!(!parser.events().iter().any(|e| matches!(
            e,
            AgentStateEvent::TextDelta { text, .. } if text == "leaked"
        )));
    }

    #[test]
    fn design_openai_rows_map_to_text_and_tools() {
        let raw = concat!(
            r#"{"role":"assistant","content":"hello"}"#,
            "\n",
            r#"{"role":"assistant","reasoning_content":"why"}"#,
            "\n",
            r#"{"role":"assistant","tool_calls":[{"id":"call_1","type":"function","function":{"name":"bash","arguments":"{\"command\":\"ls\"}"}}]}"#,
            "\n",
            r#"{"role":"tool","tool_call_id":"call_1","content":"a\nb\n"}"#,
            "\n",
        );
        let parser = parse(raw, Some(0));
        assert_eq!(parser.summary().text, "hello");
        assert_eq!(parser.summary().reasoning, "why");
        assert_eq!(parser.summary().tool_calls.len(), 1);
        assert_eq!(parser.summary().tool_calls[0].name, "bash");
        assert_eq!(
            parser.summary().tool_calls[0].output,
            Some(serde_json::json!("a\nb\n"))
        );
        assert!(parser.summary().tool_calls[0].ok);
        assert!(parser.events().iter().any(|e| matches!(
            e,
            AgentStateEvent::ToolStarted { id, name, input }
                if id == "call_1" && name == "bash"
                    && *input == serde_json::json!({ "command": "ls" })
        )));
        assert!(parser.events().iter().any(|e| matches!(
            e,
            AgentStateEvent::ToolFinished { id, name, ok: true, .. }
                if id == "call_1" && name == "bash"
        )));
    }

    #[test]
    fn missing_session_id_degrades_gracefully() {
        let raw = concat!(
            r#"{"role":"assistant","content":"hello"}"#,
            "\n",
            r#"{"type":"result","is_error":false,"result":"hello"}"#,
            "\n",
        );
        let parser = parse(raw, Some(0));
        assert!(parser.summary().session_id.is_none());
        assert_eq!(parser.summary().text, "hello");
        assert_eq!(parser.summary().outcome, Some(IdleOutcome::Succeeded));
    }

    #[test]
    fn noise_and_partial_lines_are_tolerated() {
        let noisy = format!(
            "warning: something happened\n\n{{ not json\n{{\"hello\":\"world\"}}\n{}",
            GROUNDED
        );
        let noisy = parse(&noisy, Some(0));
        let clean = parse(GROUNDED, Some(0));
        assert_eq!(noisy.events(), clean.events());
        assert_eq!(noisy.summary(), clean.summary());
    }

    #[test]
    fn partial_line_split_across_chunks_is_reassembled() {
        let mut parser = VibeStreamParser::new();
        let bytes = GROUNDED.as_bytes();
        let split = bytes.len() / 2;
        parser.push(&bytes[..split]);
        parser.push(&bytes[split..]);
        parser.finish(Some(0));
        let clean = parse(GROUNDED, Some(0));
        assert_eq!(parser.events(), clean.events());
        assert_eq!(parser.summary(), clean.summary());
    }

    #[test]
    fn truncated_stream_is_interrupted() {
        // Only reasoning arrived: no assistant text, tool, or `result` proves the
        // turn completed, so the truncated observation is not a clean success.
        let raw = concat!(
            r#"{"id":"a","sessionId":"s","turnId":"t","type":"reasoning","text":"thinking…"}"#,
            "\n",
        );
        let parser = parse(raw, Some(0));
        assert_eq!(parser.summary().reasoning, "thinking…");
        assert_eq!(parser.summary().outcome, Some(IdleOutcome::Interrupted));
        assert_eq!(
            parser.events().last(),
            Some(&AgentStateEvent::Idle {
                outcome: IdleOutcome::Interrupted,
            })
        );
    }

    #[test]
    fn plain_text_leaves_outcome_unset() {
        let parser = parse("plain output\nmore text\n", Some(0));
        assert!(parser.summary().outcome.is_none());
        assert!(parser.events().is_empty());
    }

    #[test]
    fn error_row_sets_failed_outcome() {
        let raw = concat!(
            r#"{"id":"a","sessionId":"s","turnId":"t","type":"message","role":"assistant","content":[{"type":"text","text":"partial"}]}"#,
            "\n",
            r#"{"type":"error","error":{"message":"rate limited"}}"#,
            "\n",
        );
        let parser = parse(raw, Some(0));
        assert_eq!(parser.summary().error.as_deref(), Some("rate limited"));
        assert_eq!(parser.summary().outcome, Some(IdleOutcome::Failed));
    }

    #[test]
    fn finish_is_idempotent() {
        let mut parser = VibeStreamParser::new();
        parser.push(GROUNDED.as_bytes());
        parser.finish(Some(0));
        parser.finish(Some(1));
        let idles = parser
            .events()
            .iter()
            .filter(|e| matches!(e, AgentStateEvent::Idle { .. }))
            .count();
        assert_eq!(idles, 1);
        assert_eq!(parser.summary().outcome, Some(IdleOutcome::Succeeded));
    }

    #[test]
    fn finish_backfills_the_session_from_the_index() {
        let dir = std::env::temp_dir().join(format!(
            "favetto-vibe-stream-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let sessions = dir.join("logs").join("session");
        std::fs::create_dir_all(&sessions).unwrap();
        let since = DateTime::parse_from_rfc3339("2026-09-24T10:00:00+00:00")
            .unwrap()
            .with_timezone(&Utc);
        std::fs::write(
            sessions.join(".session_index.json"),
            serde_json::to_vec(&serde_json::json!({
                "session_20260924_100005_abcd": {
                    "session_id": "ses_index",
                    "cwd": "/home/me/proj",
                    "start_time": "2026-09-24T10:00:05+00:00",
                    "mtime_ns": 1_i64,
                    "title": "From the index"
                }
            }))
            .unwrap(),
        )
        .unwrap();

        // No `sessionId` on the rows: only the index can supply it.
        let mut parser =
            VibeStreamParser::new().with_index(dir.clone(), PathBuf::from("/home/me/proj"), since);
        parser.push(
            br#"{"id":"a","turnId":"t","type":"message","role":"assistant","content":[{"type":"text","text":"hi"}]}"#,
        );
        parser.push(b"\n");
        parser.finish(Some(0));

        assert_eq!(parser.summary().session_id.as_deref(), Some("ses_index"));
        assert_eq!(parser.summary().title.as_deref(), Some("From the index"));
        assert!(parser.events().iter().any(|e| matches!(
            e,
            AgentStateEvent::Session { session_id: Some(id), title: Some(title), .. }
                if id == "ses_index" && title == "From the index"
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stdout_parser_trait_round_trips() {
        let mut parser = VibeStreamParser::new();
        StdoutParser::push(&mut parser, GROUNDED.as_bytes());
        StdoutParser::finish(&mut parser, Some(0));
        let summary = StdoutParser::summary(&parser);
        assert_eq!(summary.outcome, Some(IdleOutcome::Succeeded));
        assert!(!summary.text.is_empty());
    }

    #[test]
    fn state_source_requires_headless_streaming() {
        let base = StateSourceConfig {
            headless: true,
            args: vec![
                "-p".to_string(),
                "--output".to_string(),
                "streaming".to_string(),
            ],
            ..Default::default()
        };
        assert!(state_source(&base).is_some());
        assert!(state_source(&StateSourceConfig {
            headless: false,
            ..base.clone()
        })
        .is_none());
        assert!(state_source(&StateSourceConfig {
            headless: true,
            args: vec!["-p".to_string(), "--auto-approve".to_string()],
            ..base
        })
        .is_none());
    }

    #[tokio::test]
    async fn source_start_feeds_events_from_stdout() {
        let source = VibeStreamSource { cwd: None };
        let ctx = StateContext {
            favetto_session: "fav".to_string(),
            external_session: None,
            prompt: None,
            headless: true,
            cwd: None,
            program: PathBuf::from("vibe"),
            args: vec!["--output".to_string(), "streaming".to_string()],
            env: Default::default(),
            stdin: Arc::new(parking_lot::Mutex::new(Box::new(std::io::sink()))),
        };
        let start = source.start(ctx).unwrap();
        assert_eq!(source.label(), "vibe-streaming");
        let mut stdout = start.stdout.unwrap();
        stdout.push(GROUNDED.as_bytes());
        stdout.finish(Some(0));
        let mut events = start.events;
        let first = events.recv().await.unwrap();
        assert_eq!(first, AgentStateEvent::TurnStarted);
    }
}
