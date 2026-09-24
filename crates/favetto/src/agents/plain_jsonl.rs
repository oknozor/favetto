//! Permissive parser for a custom agent's `plain-jsonl` output.
//!
//! Unlike the built-in adapters, this parser makes no assumption about a CLI's
//! schema. It accepts any JSON object per line and extracts the obvious fields:
//! a top-level string `text`/`content` becomes a [`TextDelta`], a
//! `{"type":"tool_use"}`-shaped row becomes a tool invocation, and a row that
//! carries an error becomes an error. Everything else is preserved verbatim as
//! an opaque [`ToolCall`] so no structured row is silently dropped.
//!
//! It backs `output_format = "plain-jsonl"` for a `configurable` agent and is
//! exposed through [`ConfigurableAgent::parse_output`]. See
//! `docs/design/agent-state-adapters.md` §7.5.
//!
//! [`TextDelta`]: AgentStateEvent::TextDelta
//! [`ConfigurableAgent::parse_output`]: super::configurable::ConfigurableAgent

use favetto_core::model::{AgentStateEvent, IdleOutcome, MessageRole, RunSummary, ToolCall};

use crate::agents::agent::{extract_session_id, SessionIdProbe};

/// The `output_format` value that selects this parser.
pub(crate) const PLAIN_JSONL: &str = "plain-jsonl";

/// A streaming, schema-tolerant `plain-jsonl` parser.
pub(crate) struct PlainJsonlParser {
    /// Bytes seen but not yet terminated by a newline.
    buffer: Vec<u8>,
    probe: Option<SessionIdProbe>,
    events: Vec<AgentStateEvent>,
    summary: RunSummary,
    /// At least one JSON object row was seen (so the stream is machine-readable).
    saw_structured: bool,
    /// An `error` row was seen.
    saw_error: bool,
    finished: bool,
}

impl PlainJsonlParser {
    pub(crate) fn new(probe: Option<SessionIdProbe>) -> Self {
        Self {
            buffer: Vec::new(),
            probe,
            events: Vec::new(),
            summary: RunSummary::default(),
            saw_structured: false,
            saw_error: false,
            finished: false,
        }
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
    /// `plain-jsonl` has no explicit completion marker, so a stream that
    /// produced at least one JSON row and exited cleanly is reported as
    /// succeeded; a non-zero exit or an error row makes it failed.
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
            // No machine-readable rows: leave `outcome` unset and emit nothing,
            // exactly as an unstructured run did before.
            return;
        }
        let outcome = if self.saw_error || matches!(exit_code, Some(code) if code != 0) {
            IdleOutcome::Failed
        } else {
            IdleOutcome::Succeeded
        };
        self.summary.outcome = Some(outcome);
        self.events.push(AgentStateEvent::Idle { outcome });
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

    fn process_line(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            // Noise (a warning, spinner, partial write): ignore it.
            return;
        };
        let Some(obj) = value.as_object() else {
            return;
        };
        self.saw_structured = true;

        // The first row that carries the agent's session id announces it.
        if self.summary.session_id.is_none() {
            if let Some(probe) = &self.probe {
                if let Some(session_id) = extract_session_id(&value, probe) {
                    self.summary.session_id = Some(session_id.clone());
                    self.events.push(AgentStateEvent::Session {
                        session_id: Some(session_id),
                        title: None,
                        model: None,
                    });
                }
            }
        }

        let kind = obj.get("type").and_then(|v| v.as_str()).unwrap_or_default();
        if kind == "error" {
            self.process_error(obj);
            return;
        }
        if is_tool_kind(kind) {
            self.process_tool(obj, kind);
            return;
        }
        if let Some(text) = top_level_text(obj) {
            if !text.is_empty() {
                self.summary.text.push_str(text);
                self.events.push(AgentStateEvent::TextDelta {
                    role: role_from(obj),
                    text: text.to_string(),
                });
            }
            return;
        }

        // Everything else is preserved verbatim as an opaque tool-call entry.
        self.summary.tool_calls.push(ToolCall {
            id: first_str(obj, TOOL_ID_KEYS).unwrap_or_default().to_string(),
            name: opaque_name(obj, kind),
            input: value,
            ok: false,
            output: None,
        });
    }

    fn process_error(&mut self, obj: &serde_json::Map<String, serde_json::Value>) {
        let message = ["message", "error", "text"]
            .iter()
            .find_map(|key| obj.get(*key).and_then(|v| v.as_str()))
            .unwrap_or("agent error")
            .to_string();
        self.saw_error = true;
        self.summary.error = Some(message.clone());
        self.events.push(AgentStateEvent::Error { message });
    }

    fn process_tool(&mut self, obj: &serde_json::Map<String, serde_json::Value>, kind: &str) {
        let id = first_str(obj, TOOL_ID_KEYS).unwrap_or_default().to_string();
        let name = first_str(obj, TOOL_NAME_KEYS).unwrap_or(kind).to_string();
        let input = first_value(obj, TOOL_INPUT_KEYS)
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let output = first_value(obj, TOOL_OUTPUT_KEYS).cloned();
        let ok = !tool_failed(obj);
        self.summary.tool_calls.push(ToolCall {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
            ok,
            output: output.clone(),
        });
        self.events.push(AgentStateEvent::ToolStarted {
            id: id.clone(),
            name: name.clone(),
            input,
        });
        self.events.push(AgentStateEvent::ToolFinished {
            id,
            name,
            ok,
            output,
        });
    }
}

impl crate::agents::state::StdoutParser for PlainJsonlParser {
    fn push(&mut self, chunk: &[u8]) {
        PlainJsonlParser::push(self, chunk);
    }

    fn finish(&mut self, exit_code: Option<i32>) {
        PlainJsonlParser::finish(self, exit_code);
    }

    fn summary(&self) -> RunSummary {
        PlainJsonlParser::summary(self).clone()
    }
}

const TOOL_ID_KEYS: &[&str] = &["id", "tool_use_id", "tool_call_id", "callID", "call_id"];
const TOOL_NAME_KEYS: &[&str] = &["name", "tool", "tool_name"];
const TOOL_INPUT_KEYS: &[&str] = &["input", "arguments", "parameters"];
const TOOL_OUTPUT_KEYS: &[&str] = &["output", "result", "content"];

/// `type` values treated as a tool invocation.
fn is_tool_kind(kind: &str) -> bool {
    matches!(kind, "tool_use" | "tool_call" | "tool_result" | "tool")
}

/// The first top-level string `text` or `content`.
fn top_level_text(obj: &serde_json::Map<String, serde_json::Value>) -> Option<&str> {
    ["text", "content"]
        .iter()
        .find_map(|key| obj.get(*key).and_then(|v| v.as_str()))
}

fn role_from(obj: &serde_json::Map<String, serde_json::Value>) -> MessageRole {
    match obj.get("role").and_then(|v| v.as_str()) {
        Some("system") => MessageRole::System,
        Some("user") => MessageRole::User,
        Some("tool") => MessageRole::Tool,
        _ => MessageRole::Assistant,
    }
}

fn first_str<'a>(
    obj: &'a serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| obj.get(*key).and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty())
}

fn first_value<'a>(
    obj: &'a serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> Option<&'a serde_json::Value> {
    keys.iter().find_map(|key| obj.get(*key))
}

/// Whether a tool row marks itself as failed.
fn tool_failed(obj: &serde_json::Map<String, serde_json::Value>) -> bool {
    if obj.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
        return true;
    }
    if let Some(status) = obj.get("status").and_then(|v| v.as_str()) {
        if matches!(status, "error" | "failed" | "failure") {
            return true;
        }
    }
    match obj.get("error") {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Bool(failed)) => *failed,
        Some(_) => true,
    }
}

/// A human-readable name for a preserved opaque row.
fn opaque_name(obj: &serde_json::Map<String, serde_json::Value>, kind: &str) -> String {
    if !kind.is_empty() {
        return kind.to_string();
    }
    first_str(obj, &["name", "role"])
        .map(str::to_string)
        .unwrap_or_else(|| "opaque".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parser() -> PlainJsonlParser {
        PlainJsonlParser::new(Some(SessionIdProbe::JsonKey("session_id".to_string())))
    }

    fn parse(raw: &str, exit_code: Option<i32>) -> PlainJsonlParser {
        let mut parser = parser();
        parser.push(raw.as_bytes());
        parser.finish(exit_code);
        parser
    }

    #[test]
    fn text_content_and_session_id_are_captured() {
        let raw = concat!(
            r#"{"session_id":"ses_1","type":"message","role":"assistant","text":"hello"}"#,
            "\n",
            r#"{"content":" world"}"#,
            "\n",
            "not json\n",
        );
        let parser = parse(raw, Some(0));
        let summary = parser.summary();
        assert_eq!(summary.session_id.as_deref(), Some("ses_1"));
        assert_eq!(summary.text, "hello world");
        assert_eq!(summary.outcome, Some(IdleOutcome::Succeeded));
        assert_eq!(
            parser.events(),
            &[
                AgentStateEvent::Session {
                    session_id: Some("ses_1".to_string()),
                    title: None,
                    model: None,
                },
                AgentStateEvent::TextDelta {
                    role: MessageRole::Assistant,
                    text: "hello".to_string(),
                },
                AgentStateEvent::TextDelta {
                    role: MessageRole::Assistant,
                    text: " world".to_string(),
                },
                AgentStateEvent::Idle {
                    outcome: IdleOutcome::Succeeded,
                },
            ]
        );
    }

    #[test]
    fn tool_use_rows_become_tool_calls_and_events() {
        let raw =
            r#"{"type":"tool_use","id":"c1","name":"bash","input":{"command":"ls"},"output":"ok"}"#;
        let parser = parse(raw, Some(0));
        let summary = parser.summary();
        assert_eq!(summary.tool_calls.len(), 1);
        let call = &summary.tool_calls[0];
        assert_eq!(call.id, "c1");
        assert_eq!(call.name, "bash");
        assert_eq!(call.input, serde_json::json!({"command": "ls"}));
        assert!(call.ok);
        assert_eq!(call.output, Some(serde_json::json!("ok")));
        assert_eq!(
            parser.events(),
            &[
                AgentStateEvent::ToolStarted {
                    id: "c1".to_string(),
                    name: "bash".to_string(),
                    input: serde_json::json!({"command": "ls"}),
                },
                AgentStateEvent::ToolFinished {
                    id: "c1".to_string(),
                    name: "bash".to_string(),
                    ok: true,
                    output: Some(serde_json::json!("ok")),
                },
                AgentStateEvent::Idle {
                    outcome: IdleOutcome::Succeeded,
                },
            ]
        );
    }

    #[test]
    fn unknown_rows_are_preserved_as_opaque_tool_calls() {
        let raw = concat!(
            r#"{"type":"progress","step":2}"#,
            "\n",
            r#"{"foo":"bar"}"#,
            "\n",
        );
        let parser = parse(raw, Some(0));
        let summary = parser.summary();
        assert_eq!(summary.tool_calls.len(), 2);
        assert_eq!(summary.tool_calls[0].name, "progress");
        assert_eq!(
            summary.tool_calls[0].input,
            serde_json::json!({"type": "progress", "step": 2})
        );
        assert!(!summary.tool_calls[0].ok);
        assert_eq!(summary.tool_calls[1].name, "opaque");
    }

    #[test]
    fn error_rows_and_failed_exit_set_failed_outcome() {
        let parser = parse(r#"{"type":"error","message":"boom"}"#, Some(1));
        let summary = parser.summary();
        assert_eq!(summary.error.as_deref(), Some("boom"));
        assert_eq!(summary.outcome, Some(IdleOutcome::Failed));

        // A clean stream with a non-zero exit is still a failure.
        let parser = parse(r#"{"text":"partial"}"#, Some(2));
        assert_eq!(parser.summary().outcome, Some(IdleOutcome::Failed));
    }

    #[test]
    fn empty_or_unstructured_stream_has_no_outcome() {
        let parser = parse("only noise\n", Some(0));
        let summary = parser.summary();
        assert!(summary.outcome.is_none());
        assert!(summary.text.is_empty());
        assert!(summary.tool_calls.is_empty());
    }

    #[test]
    fn parser_implements_stdout_parser() {
        let mut parser = PlainJsonlParser::new(None);
        crate::agents::state::StdoutParser::push(&mut parser, b"{\"text\":\"hi\"}\n");
        crate::agents::state::StdoutParser::finish(&mut parser, Some(0));
        let summary = crate::agents::state::StdoutParser::summary(&parser);
        assert_eq!(summary.text, "hi");
        assert_eq!(summary.outcome, Some(IdleOutcome::Succeeded));
    }
}
