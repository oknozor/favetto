//! Tolerant parser for pi's JSON and RPC stdout streams.
//!
//! pi speaks a line-delimited JSON protocol in both `--mode json` (one-shot) and
//! `--mode rpc` (long-lived). The session-event shapes are shared; RPC adds
//! `response` records and `extension_ui_request` dialogs. This parser normalizes
//! both dialects into [`AgentStateEvent`]s and folds the stream into a
//! [`RunSummary`].
//!
//! The reader thread feeds raw PTY bytes via [`StdoutParser::push`]; for a
//! headless RPC run the parser also closes pi's stdin (`0x04`, the same EOF
//! mechanism as `stdin_eof`) once the session settles, so a long-lived RPC
//! process exits and the task lifecycle completes.

use std::io::Write;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::mpsc;

use favetto_core::model::{
    AgentStateEvent, AgentUsage, AwaitingInputKind, IdleOutcome, InputRequest, MessageRole,
    RunSummary, ToolCall,
};

use crate::agents::state::StdoutParser;

/// A shared handle to the agent's stdin (the PTY master writer).
pub(crate) type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// A streaming, noise-tolerant `pi-json` / `pi-rpc` parser.
pub(crate) struct PiJsonlParser {
    /// Bytes seen but not yet terminated by a newline.
    buffer: Vec<u8>,
    /// Every event observed, in order (kept for tests and `parse_output`).
    events: Vec<AgentStateEvent>,
    /// When present, every event is forwarded here as it is observed.
    tx: Option<mpsc::UnboundedSender<AgentStateEvent>>,
    /// When present, EOT is written here once the session settles, so a headless
    /// RPC process exits instead of waiting forever for input.
    close_on_settle: Option<SharedWriter>,
    summary: RunSummary,
    /// The latest cumulative provider usage, emitted once per `turn_end`.
    last_usage: Option<AgentUsage>,
    /// At least one recognized record was seen (the stream is machine-readable).
    saw_structured: bool,
    /// `agent_settled` was seen (pi will not continue automatically).
    saw_settled: bool,
    /// An error record was seen.
    saw_error: bool,
    finished: bool,
}

impl PiJsonlParser {
    /// A parser that only accumulates events and the summary (`parse_output`).
    pub(crate) fn new() -> Self {
        Self {
            buffer: Vec::new(),
            events: Vec::new(),
            tx: None,
            close_on_settle: None,
            summary: RunSummary::default(),
            last_usage: None,
            saw_structured: false,
            saw_settled: false,
            saw_error: false,
            finished: false,
        }
    }

    /// A parser that forwards events to `tx` and closes `stdin` (EOT) on settle.
    pub(crate) fn with_channel(
        tx: mpsc::UnboundedSender<AgentStateEvent>,
        close_on_settle: Option<SharedWriter>,
    ) -> Self {
        Self {
            tx: Some(tx),
            close_on_settle,
            ..Self::new()
        }
    }

    /// The structured result built so far.
    pub(crate) fn summary(&self) -> &RunSummary {
        &self.summary
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
    /// `exit_code` is the child's status; it is only a tie-breaker, never used to
    /// claim success for a stream that never settled.
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
            // No machine-readable records: leave `outcome` unset, exactly as an
            // unstructured run did before.
            return;
        }
        if !self.saw_settled {
            let outcome = if self.saw_error || matches!(exit_code, Some(code) if code != 0) {
                IdleOutcome::Failed
            } else {
                // A truncated stream (the process died before `agent_settled`)
                // must not be reported as a clean success.
                IdleOutcome::Interrupted
            };
            self.summary.outcome = Some(outcome);
            self.emit(AgentStateEvent::Idle { outcome });
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
        let Some(obj) = value.as_object() else {
            return;
        };
        match obj.get("type").and_then(|v| v.as_str()).unwrap_or_default() {
            "session" => self.process_session_header(&value),
            "response" => self.process_response(&value),
            "agent_start" => {
                self.saw_structured = true;
                self.emit(AgentStateEvent::TurnStarted);
            }
            "message_update" => self.process_message_update(&value),
            "tool_execution_start" => {
                self.saw_structured = true;
                let id = json_str(&value, "/toolCallId").to_string();
                let name = json_str(&value, "/toolName").to_string();
                let input = value
                    .get("args")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                self.emit(AgentStateEvent::ToolStarted { id, name, input });
            }
            "tool_execution_update" => {
                self.saw_structured = true;
                let id = json_str(&value, "/toolCallId").to_string();
                let partial = value
                    .get("partialResult")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                self.emit(AgentStateEvent::ToolUpdated { id, partial });
            }
            "tool_execution_end" => self.process_tool_end(&value),
            "extension_ui_request" => self.process_extension_ui(&value),
            "session_info_changed" => {
                let name = json_str(&value, "/name").trim().to_string();
                if !name.is_empty() {
                    self.saw_structured = true;
                    self.summary.title = Some(name.clone());
                    self.emit(AgentStateEvent::Title { title: name });
                }
            }
            "turn_end" => self.process_turn_end(),
            "agent_settled" => self.process_settled(),
            "extension_error" => {
                let message = json_str(&value, "/error").trim().to_string();
                let message = if message.is_empty() {
                    "extension error".to_string()
                } else {
                    message
                };
                self.record_error(message);
            }
            // Echoed commands (the PTY can echo our writes) and unrelated
            // records: `prompt`, `get_state`, `get_messages`, `abort`,
            // `extension_ui_response`, `agent_end`, `turn_start`, … are ignored.
            _ => {}
        }
    }

    /// The one-shot `--mode json` session header: `{"type":"session","id":…}`.
    fn process_session_header(&mut self, value: &serde_json::Value) {
        let Some(session_id) = value.get("id").and_then(|v| v.as_str()) else {
            return;
        };
        self.saw_structured = true;
        if self.summary.session_id.is_none() {
            self.summary.session_id = Some(session_id.to_string());
        }
        self.emit(AgentStateEvent::Session {
            session_id: Some(session_id.to_string()),
            title: self.summary.title.clone(),
            model: None,
        });
    }

    /// `get_state` (and other command) responses carry the canonical session
    /// identity when RPC mode is used.
    fn process_response(&mut self, value: &serde_json::Value) {
        if value.get("success").and_then(|v| v.as_bool()) != Some(true) {
            return;
        }
        if value.get("command").and_then(|v| v.as_str()) != Some("get_state") {
            return;
        }
        let Some(data) = value.get("data") else {
            return;
        };
        let session_id = data
            .get("sessionId")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let title = data
            .get("sessionName")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let model = data
            .get("model")
            .and_then(|m| m.get("id"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        self.saw_structured = true;
        if self.summary.session_id.is_none() {
            self.summary.session_id = session_id.clone();
        }
        if title.is_some() {
            self.summary.title = title.clone();
        }
        self.emit(AgentStateEvent::Session {
            session_id,
            title,
            model,
        });
    }

    fn process_message_update(&mut self, value: &serde_json::Value) {
        if let Some(usage) = value.get("usage") {
            self.last_usage = Some(parse_usage(usage));
        }
        let kind = json_str(value, "/assistantMessageEvent/type");
        match kind {
            "text_delta" => {
                let delta = json_str(value, "/assistantMessageEvent/delta");
                if !delta.is_empty() {
                    self.saw_structured = true;
                    self.summary.text.push_str(delta);
                    self.emit(AgentStateEvent::TextDelta {
                        role: MessageRole::Assistant,
                        text: delta.to_string(),
                    });
                }
            }
            "thinking_delta" => {
                let delta = json_str(value, "/assistantMessageEvent/delta");
                if !delta.is_empty() {
                    self.saw_structured = true;
                    self.summary.reasoning.push_str(delta);
                    self.emit(AgentStateEvent::ReasoningDelta {
                        text: delta.to_string(),
                    });
                }
            }
            "error" => {
                let message = json_str(value, "/assistantMessageEvent/error");
                let message = if message.is_empty() {
                    json_str(value, "/error")
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
            _ => {}
        }
    }

    fn process_tool_end(&mut self, value: &serde_json::Value) {
        self.saw_structured = true;
        let id = json_str(value, "/toolCallId").to_string();
        let name = json_str(value, "/toolName").to_string();
        let ok = value.get("isError").and_then(|v| v.as_bool()) != Some(true);
        let output = value.get("result").cloned();
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

    fn process_extension_ui(&mut self, value: &serde_json::Value) {
        let id = json_str(value, "/id").to_string();
        match json_str(value, "/method") {
            "select" => {
                let options: Vec<String> = value
                    .get("options")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let allow_always = options
                    .iter()
                    .any(|o| o.to_ascii_lowercase().contains("always"));
                self.saw_structured = true;
                self.emit(AgentStateEvent::InputRequested {
                    request: InputRequest {
                        id,
                        kind: AwaitingInputKind::Choice,
                        message: ui_message(value),
                        options,
                        allow_always,
                    },
                });
            }
            "confirm" => {
                self.saw_structured = true;
                self.emit(AgentStateEvent::InputRequested {
                    request: InputRequest {
                        id,
                        kind: AwaitingInputKind::Confirmation,
                        message: ui_message(value),
                        options: Vec::new(),
                        allow_always: false,
                    },
                });
            }
            "input" | "editor" => {
                self.saw_structured = true;
                self.emit(AgentStateEvent::InputRequested {
                    request: InputRequest {
                        id,
                        kind: AwaitingInputKind::Other,
                        message: ui_message(value),
                        options: Vec::new(),
                        allow_always: false,
                    },
                });
            }
            "setTitle" => {
                let title = json_str(value, "/title").trim().to_string();
                if !title.is_empty() {
                    self.saw_structured = true;
                    self.emit(AgentStateEvent::Title { title });
                }
            }
            // Fire-and-forget UI (`notify`, `setStatus`, `setWidget`,
            // `set_editor_text`) carries no actionable state.
            _ => {}
        }
    }

    fn process_turn_end(&mut self) {
        // `message_update.usage` is cumulative for the response; emit it once at
        // the turn boundary so the folded summary sums turns rather than
        // re-counting the same running total.
        if let Some(usage) = self.last_usage.take() {
            self.saw_structured = true;
            self.summary.usage.merge(&usage);
            self.emit(AgentStateEvent::Usage { usage });
        }
    }

    fn process_settled(&mut self) {
        self.saw_structured = true;
        self.saw_settled = true;
        let outcome = if self.saw_error {
            IdleOutcome::Failed
        } else {
            IdleOutcome::Succeeded
        };
        self.summary.outcome = Some(outcome);
        self.emit(AgentStateEvent::Idle { outcome });
        // Close pi's stdin so a long-lived RPC process exits. `0x04` is the same
        // EOF signal the manager sends for `stdin_eof`; send it at most once.
        if let Some(writer) = self.close_on_settle.take() {
            let mut w = writer.lock();
            let _ = w.write_all(&[0x04]);
            let _ = w.flush();
        }
    }

    fn record_error(&mut self, message: String) {
        self.saw_structured = true;
        self.saw_error = true;
        self.summary.error = Some(message.clone());
        self.emit(AgentStateEvent::Error { message });
    }
}

impl StdoutParser for PiJsonlParser {
    fn push(&mut self, chunk: &[u8]) {
        PiJsonlParser::push(self, chunk);
    }

    fn finish(&mut self, exit_code: Option<i32>) {
        PiJsonlParser::finish(self, exit_code);
    }

    fn summary(&self) -> RunSummary {
        self.summary.clone()
    }
}

/// The human-readable prompt for a dialog: the title, falling back to `message`.
fn ui_message(value: &serde_json::Value) -> String {
    let title = json_str(value, "/title").trim();
    if !title.is_empty() {
        return title.to_string();
    }
    let message = json_str(value, "/message").trim();
    if !message.is_empty() {
        return message.to_string();
    }
    String::new()
}

/// Parse pi's usage object into an [`AgentUsage`].
fn parse_usage(value: &serde_json::Value) -> AgentUsage {
    AgentUsage {
        input_tokens: u64_at(value, "/input"),
        output_tokens: u64_at(value, "/output"),
        cache_read_tokens: u64_at(value, "/cacheRead"),
        cache_write_tokens: u64_at(value, "/cacheWrite"),
        cost_usd: value.pointer("/cost/total").and_then(|v| v.as_f64()),
        ..Default::default()
    }
}

/// Read a nested string, returning `""` when the pointer is missing.
fn json_str<'a>(value: &'a serde_json::Value, pointer: &str) -> &'a str {
    value
        .pointer(pointer)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
}

/// Read a nested `u64`, returning `0` when the pointer is missing.
fn u64_at(value: &serde_json::Value, pointer: &str) -> u64 {
    value.pointer(pointer).and_then(|v| v.as_u64()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Write` that appends to a shared buffer a test can inspect.
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn shared_writer() -> (SharedWriter, Arc<Mutex<Vec<u8>>>) {
        let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(SharedBuffer(buffer.clone()))));
        (writer, buffer)
    }

    fn parse(raw: &str, exit_code: Option<i32>) -> PiJsonlParser {
        let mut parser = PiJsonlParser::new();
        parser.push(raw.as_bytes());
        parser.finish(exit_code);
        parser
    }

    const HAPPY: &str = concat!(
        r#"{"type":"response","command":"get_state","success":true,"data":{"sessionId":"ses_pi","sessionName":"Fix it","model":{"id":"claude-sonnet-4"}}}"#,
        "\n",
        r#"{"type":"agent_start"}"#,
        "\n",
        r#"{"type":"message_update","usage":{"input":100,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":101,"cost":{"total":0.001}},"assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":"hmm"}}"#,
        "\n",
        r#"{"type":"message_update","usage":{"input":100,"output":20,"cacheRead":10,"cacheWrite":5,"totalTokens":135,"cost":{"total":0.02}},"assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"hello"}}"#,
        "\n",
        r#"{"type":"tool_execution_start","toolCallId":"call_1","toolName":"bash","args":{"command":"echo hi"}}"#,
        "\n",
        r#"{"type":"tool_execution_update","toolCallId":"call_1","toolName":"bash","args":{"command":"echo hi"},"partialResult":{"content":"hi"}}"#,
        "\n",
        r#"{"type":"tool_execution_end","toolCallId":"call_1","toolName":"bash","result":{"content":"hi\n"},"isError":false}"#,
        "\n",
        r#"{"type":"turn_end","message":{},"toolResults":[]}"#,
        "\n",
        r#"{"type":"agent_settled"}"#,
        "\n",
        r#"{"type":"agent_end","messages":[],"willRetry":false}"#,
        "\n",
    );

    #[test]
    fn rpc_happy_path_maps_session_text_tools_usage_and_idle() {
        let parser = parse(HAPPY, Some(0));
        assert_eq!(
            parser.events(),
            &[
                AgentStateEvent::Session {
                    session_id: Some("ses_pi".to_string()),
                    title: Some("Fix it".to_string()),
                    model: Some("claude-sonnet-4".to_string()),
                },
                AgentStateEvent::TurnStarted,
                AgentStateEvent::ReasoningDelta {
                    text: "hmm".to_string(),
                },
                AgentStateEvent::TextDelta {
                    role: MessageRole::Assistant,
                    text: "hello".to_string(),
                },
                AgentStateEvent::ToolStarted {
                    id: "call_1".to_string(),
                    name: "bash".to_string(),
                    input: serde_json::json!({ "command": "echo hi" }),
                },
                AgentStateEvent::ToolUpdated {
                    id: "call_1".to_string(),
                    partial: serde_json::json!({ "content": "hi" }),
                },
                AgentStateEvent::ToolFinished {
                    id: "call_1".to_string(),
                    name: "bash".to_string(),
                    ok: true,
                    output: Some(serde_json::json!({ "content": "hi\n" })),
                },
                AgentStateEvent::Usage {
                    usage: AgentUsage {
                        input_tokens: 100,
                        output_tokens: 20,
                        cache_read_tokens: 10,
                        cache_write_tokens: 5,
                        cost_usd: Some(0.02),
                        ..Default::default()
                    },
                },
                AgentStateEvent::Idle {
                    outcome: IdleOutcome::Succeeded,
                },
            ]
        );

        let summary = parser.summary();
        assert_eq!(summary.session_id.as_deref(), Some("ses_pi"));
        assert_eq!(summary.title.as_deref(), Some("Fix it"));
        assert_eq!(summary.text, "hello");
        assert_eq!(summary.reasoning, "hmm");
        assert_eq!(summary.tool_calls.len(), 1);
        assert_eq!(summary.tool_calls[0].name, "bash");
        assert!(summary.tool_calls[0].ok);
        assert_eq!(summary.usage.input_tokens, 100);
        assert_eq!(summary.usage.output_tokens, 20);
        assert_eq!(summary.usage.cache_read_tokens, 10);
        assert_eq!(summary.usage.cache_write_tokens, 5);
        assert_eq!(summary.usage.cost_usd, Some(0.02));
        assert_eq!(summary.outcome, Some(IdleOutcome::Succeeded));
        assert!(summary.error.is_none());
    }

    #[test]
    fn json_mode_session_header_yields_session_id() {
        let raw = concat!(
            r#"{"type":"session","version":3,"id":"ses_json","timestamp":"2024-12-03T14:00:00.000Z","cwd":"/p"}"#,
            "\n",
            r#"{"type":"agent_start"}"#,
            "\n",
            r#"{"type":"agent_settled"}"#,
            "\n",
        );
        let parser = parse(raw, Some(0));
        assert_eq!(
            parser.events().first(),
            Some(&AgentStateEvent::Session {
                session_id: Some("ses_json".to_string()),
                title: None,
                model: None,
            })
        );
        assert_eq!(parser.summary().session_id.as_deref(), Some("ses_json"));
        assert_eq!(parser.summary().outcome, Some(IdleOutcome::Succeeded));
    }

    #[test]
    fn extension_ui_request_maps_to_input_requested() {
        let raw = concat!(
            r#"{"type":"extension_ui_request","id":"ui-1","method":"select","title":"Allow dangerous command?","options":["Allow once","Always allow","Block"],"timeout":10000}"#,
            "\n",
            r#"{"type":"extension_ui_request","id":"ui-2","method":"confirm","title":"Clear session?","message":"All messages will be lost."}"#,
            "\n",
            r#"{"type":"extension_ui_request","id":"ui-3","method":"input","title":"Enter a value","placeholder":"type..."}"#,
            "\n",
            r#"{"type":"extension_ui_request","id":"ui-4","method":"setTitle","title":"pi - proj"}"#,
            "\n",
            r#"{"type":"extension_ui_request","id":"ui-5","method":"notify","message":"blocked","notifyType":"warning"}"#,
            "\n",
        );
        let parser = parse(raw, Some(0));
        assert_eq!(
            parser.events(),
            &[
                AgentStateEvent::InputRequested {
                    request: InputRequest {
                        id: "ui-1".to_string(),
                        kind: AwaitingInputKind::Choice,
                        message: "Allow dangerous command?".to_string(),
                        options: vec![
                            "Allow once".to_string(),
                            "Always allow".to_string(),
                            "Block".to_string(),
                        ],
                        allow_always: true,
                    },
                },
                AgentStateEvent::InputRequested {
                    request: InputRequest {
                        id: "ui-2".to_string(),
                        kind: AwaitingInputKind::Confirmation,
                        message: "Clear session?".to_string(),
                        options: Vec::new(),
                        allow_always: false,
                    },
                },
                AgentStateEvent::InputRequested {
                    request: InputRequest {
                        id: "ui-3".to_string(),
                        kind: AwaitingInputKind::Other,
                        message: "Enter a value".to_string(),
                        options: Vec::new(),
                        allow_always: false,
                    },
                },
                AgentStateEvent::Title {
                    title: "pi - proj".to_string(),
                },
                AgentStateEvent::Idle {
                    outcome: IdleOutcome::Interrupted,
                },
            ]
        );
    }

    #[test]
    fn session_info_changed_emits_title() {
        let parser = parse(
            r#"{"type":"session_info_changed","name":"Refactor auth"}"#,
            Some(0),
        );
        assert_eq!(
            parser.events(),
            &[
                AgentStateEvent::Title {
                    title: "Refactor auth".to_string(),
                },
                AgentStateEvent::Idle {
                    outcome: IdleOutcome::Interrupted,
                },
            ]
        );
        assert_eq!(parser.summary().title.as_deref(), Some("Refactor auth"));
    }

    #[test]
    fn echoed_commands_and_noise_are_ignored() {
        let noisy = format!(
            "warning: something happened\n\n{{ not json\n{}\n{{\"hello\":\"world\"}}\n{}",
            r#"{"id":"favetto-0","type":"get_state"}"#, HAPPY
        );
        let noisy = parse(&noisy, Some(0));
        let clean = parse(HAPPY, Some(0));
        assert_eq!(noisy.events(), clean.events());
        assert_eq!(noisy.summary(), clean.summary());
    }

    #[test]
    fn truncated_stream_is_interrupted_not_succeeded() {
        let raw = concat!(
            r#"{"type":"agent_start"}"#,
            "\n",
            r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"partial"}}"#,
            "\n",
        );
        let parser = parse(raw, Some(0));
        assert_eq!(
            parser.events().last(),
            Some(&AgentStateEvent::Idle {
                outcome: IdleOutcome::Interrupted,
            })
        );
        assert_eq!(parser.summary().outcome, Some(IdleOutcome::Interrupted));
        assert_eq!(parser.summary().text, "partial");
    }

    #[test]
    fn extension_error_sets_failed_outcome() {
        let raw = concat!(
            r#"{"type":"agent_start"}"#,
            "\n",
            r#"{"type":"extension_error","extensionPath":"/x.ts","event":"tool_call","error":"boom"}"#,
            "\n",
            r#"{"type":"agent_settled"}"#,
            "\n",
        );
        let parser = parse(raw, Some(1));
        assert_eq!(
            parser.events().last(),
            Some(&AgentStateEvent::Idle {
                outcome: IdleOutcome::Failed,
            })
        );
        assert_eq!(parser.summary().outcome, Some(IdleOutcome::Failed));
        assert_eq!(parser.summary().error.as_deref(), Some("boom"));
    }

    #[test]
    fn partial_line_split_across_chunks_is_reassembled() {
        let mut parser = PiJsonlParser::new();
        let bytes = HAPPY.as_bytes();
        let split = bytes.len() / 2;
        parser.push(&bytes[..split]);
        parser.push(&bytes[split..]);
        parser.finish(Some(0));
        let clean = parse(HAPPY, Some(0));
        assert_eq!(parser.events(), clean.events());
        assert_eq!(parser.summary(), clean.summary());
    }

    #[test]
    fn finish_is_idempotent() {
        let mut parser = PiJsonlParser::new();
        parser.push(HAPPY.as_bytes());
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
    fn plain_text_output_leaves_outcome_unset() {
        let parser = parse("plain output\nmore text\n", Some(0));
        assert!(parser.summary().outcome.is_none());
        assert!(parser.events().is_empty());
    }

    #[test]
    fn settle_writes_eot_once() {
        let (writer, buffer) = shared_writer();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut parser = PiJsonlParser::with_channel(tx, Some(writer));
        parser.push(HAPPY.as_bytes());
        parser.finish(Some(0));
        // A second settle/finish must not write another EOT.
        parser.finish(Some(0));
        let bytes = buffer.lock();
        assert_eq!(bytes.iter().filter(|b| **b == 0x04).count(), 1);
    }
}
