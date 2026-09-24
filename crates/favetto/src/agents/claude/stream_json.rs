//! Tolerant parser for `claude -p --output-format stream-json` output.
//!
//! Claude writes one JSON object per line, each with a required `type` (`system`,
//! `assistant`, `user`, `result`, and — with `--include-partial-messages` —
//! `stream_event`). A single malformed line (a warning, a spinner, a truncated
//! write) does not discard the run: non-JSON lines are skipped, recognized rows
//! are normalized to [`AgentStateEvent`]s, and the final `result` row is folded
//! into a [`RunSummary`]. A stream that produced structured rows but no `result`
//! is reported as an interrupted observation rather than a clean success, and a
//! fully unstructured stream leaves the outcome unset (the exit code decides).

use std::collections::HashMap;

use favetto_core::model::{
    AgentStateEvent, AgentUsage, IdleOutcome, MessageRole, RunSummary, ToolCall,
};

use crate::agents::state::StdoutParser;

/// A streaming, noise-tolerant `claude-stream-json` parser.
pub(crate) struct ClaudeStreamJsonParser {
    /// Bytes seen but not yet terminated by a newline.
    buffer: Vec<u8>,
    events: Vec<AgentStateEvent>,
    summary: RunSummary,
    /// At least one recognized row was seen (so the stream is machine-readable).
    saw_structured: bool,
    /// A `stream_event` delta was seen, so the completed `assistant` block would
    /// duplicate it and must be skipped.
    saw_partial: bool,
    /// A turn is in flight (so `TurnStarted` is emitted once per turn).
    in_turn: bool,
    /// The session identity has been announced.
    announced_session: bool,
    /// A `result` row was seen.
    saw_result: bool,
    /// An error row (or a failed `result`) was seen.
    saw_error: bool,
    finished: bool,
    /// tool_use id → tool name, to label a later `tool_result`.
    tool_names: HashMap<String, String>,
    /// tool_use id → index into `summary.tool_calls`, updated on `tool_result`.
    pending_tools: HashMap<String, usize>,
}

impl ClaudeStreamJsonParser {
    pub(crate) fn new() -> Self {
        Self {
            buffer: Vec::new(),
            events: Vec::new(),
            summary: RunSummary::default(),
            saw_structured: false,
            saw_partial: false,
            in_turn: false,
            announced_session: false,
            saw_result: false,
            saw_error: false,
            finished: false,
            tool_names: HashMap::new(),
            pending_tools: HashMap::new(),
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
    /// `exit_code` is only a tie-breaker: it never makes a truncated stream look
    /// successful, and a fully unstructured stream keeps `outcome` unset.
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
            return;
        }
        if self.summary.outcome.is_none() {
            let outcome = if self.saw_error || matches!(exit_code, Some(code) if code != 0) {
                IdleOutcome::Failed
            } else if self.saw_result {
                IdleOutcome::Succeeded
            } else {
                // Structured rows but no `result`: a truncated stream must not
                // be reported as a clean success.
                IdleOutcome::Interrupted
            };
            self.summary.outcome = Some(outcome);
        }
        let outcome = self.summary.outcome.unwrap_or(IdleOutcome::Interrupted);
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

    /// Emit `TurnStarted` once per turn.
    fn start_turn(&mut self) {
        if !self.in_turn {
            self.in_turn = true;
            self.events.push(AgentStateEvent::TurnStarted);
        }
    }

    /// Announce the session identity once, when a row carries one.
    fn announce_session(&mut self, session_id: &str, model: Option<String>) {
        if self.announced_session || session_id.is_empty() {
            return;
        }
        self.announced_session = true;
        self.summary.session_id = Some(session_id.to_string());
        self.events.push(AgentStateEvent::Session {
            session_id: Some(session_id.to_string()),
            title: None,
            model,
        });
    }

    fn process_line(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            return; // noise
        };
        let Some(obj) = value.as_object() else {
            return;
        };
        match obj.get("type").and_then(|v| v.as_str()).unwrap_or_default() {
            "system" => self.process_system(&value),
            "stream_event" => self.process_stream_event(&value),
            "assistant" => self.process_assistant(&value),
            "user" => self.process_user(&value),
            "result" => self.process_result(&value),
            "error" => {
                self.saw_structured = true;
                self.saw_error = true;
                let message = string_at(&value, "/error/message")
                    .or_else(|| string_at(&value, "/message"))
                    .unwrap_or_else(|| "agent error".to_string());
                self.summary.error = Some(message.clone());
                self.events.push(AgentStateEvent::Error { message });
            }
            _ => {}
        }
    }

    fn process_system(&mut self, value: &serde_json::Value) {
        match value
            .get("subtype")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
        {
            "init" => {
                self.saw_structured = true;
                let session_id = value
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let model = value
                    .get("model")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                self.announce_session(session_id, model);
            }
            "api_retry" => {
                // Informational: a retry does not make the run fail.
                self.saw_structured = true;
                let message = string_at(value, "/error")
                    .or_else(|| string_at(value, "/message"))
                    .unwrap_or_else(|| "api retry".to_string());
                self.events.push(AgentStateEvent::Error { message });
            }
            _ => {}
        }
    }

    fn process_stream_event(&mut self, value: &serde_json::Value) {
        let Some(event) = value.get("event") else {
            return;
        };
        match event
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
        {
            "message_start" => {
                self.saw_structured = true;
                self.start_turn();
            }
            "content_block_delta" => {
                let Some(delta) = event.get("delta") else {
                    return;
                };
                match delta
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                {
                    "text_delta" => {
                        let text = delta.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        if !text.is_empty() {
                            self.saw_structured = true;
                            self.saw_partial = true;
                            self.summary.text.push_str(text);
                            self.events.push(AgentStateEvent::TextDelta {
                                role: MessageRole::Assistant,
                                text: text.to_string(),
                            });
                        }
                    }
                    "thinking_delta" => {
                        let thinking = delta.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
                        if !thinking.is_empty() {
                            self.saw_structured = true;
                            self.saw_partial = true;
                            self.summary.reasoning.push_str(thinking);
                            self.events.push(AgentStateEvent::ReasoningDelta {
                                text: thinking.to_string(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn process_assistant(&mut self, value: &serde_json::Value) {
        let Some(blocks) = value.pointer("/message/content").and_then(|v| v.as_array()) else {
            return;
        };
        for block in blocks {
            match block
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
            {
                "text" => {
                    let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    if text.is_empty() {
                        continue;
                    }
                    self.saw_structured = true;
                    self.start_turn();
                    if self.saw_partial {
                        continue; // already streamed as deltas
                    }
                    self.summary.text.push_str(text);
                    self.events.push(AgentStateEvent::TextDelta {
                        role: MessageRole::Assistant,
                        text: text.to_string(),
                    });
                }
                "thinking" => {
                    let thinking = block.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
                    if thinking.is_empty() {
                        continue;
                    }
                    self.saw_structured = true;
                    self.start_turn();
                    if self.saw_partial {
                        continue;
                    }
                    self.summary.reasoning.push_str(thinking);
                    self.events.push(AgentStateEvent::ReasoningDelta {
                        text: thinking.to_string(),
                    });
                }
                "tool_use" => {
                    self.saw_structured = true;
                    self.start_turn();
                    let id = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let input = block
                        .get("input")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    self.tool_names.insert(id.clone(), name.clone());
                    let index = self.summary.tool_calls.len();
                    self.summary.tool_calls.push(ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                        ok: false,
                        output: None,
                    });
                    self.pending_tools.insert(id.clone(), index);
                    self.events
                        .push(AgentStateEvent::ToolStarted { id, name, input });
                }
                _ => {}
            }
        }
    }

    fn process_user(&mut self, value: &serde_json::Value) {
        let Some(blocks) = value.pointer("/message/content").and_then(|v| v.as_array()) else {
            return;
        };
        for block in blocks {
            if block.get("type").and_then(|v| v.as_str()) != Some("tool_result") {
                continue;
            }
            self.saw_structured = true;
            let id = block
                .get("tool_use_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let name = self.tool_names.get(&id).cloned().unwrap_or_default();
            let ok = !block
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let output = block.get("content").cloned();
            if let Some(index) = self.pending_tools.remove(&id) {
                if let Some(call) = self.summary.tool_calls.get_mut(index) {
                    call.ok = ok;
                    call.output = output.clone();
                }
            } else {
                self.summary.tool_calls.push(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    input: serde_json::Value::Null,
                    ok,
                    output: output.clone(),
                });
            }
            self.events.push(AgentStateEvent::ToolFinished {
                id,
                name,
                ok,
                output,
            });
        }
    }

    fn process_result(&mut self, value: &serde_json::Value) {
        self.saw_structured = true;
        self.saw_result = true;
        self.in_turn = false;

        if let Some(session_id) = value.get("session_id").and_then(|v| v.as_str()) {
            self.summary.session_id = Some(session_id.to_string());
            self.announce_session(session_id, None);
        }
        if let Some(text) = value.get("result").and_then(|v| v.as_str()) {
            self.summary.text = text.to_string();
        }

        let usage = AgentUsage {
            input_tokens: u64_at(value, "/usage/input_tokens"),
            output_tokens: u64_at(value, "/usage/output_tokens"),
            reasoning_tokens: 0,
            cache_read_tokens: u64_at(value, "/usage/cache_read_input_tokens"),
            cache_write_tokens: u64_at(value, "/usage/cache_creation_input_tokens"),
            cost_usd: value.get("total_cost_usd").and_then(|v| v.as_f64()),
        };
        self.summary.usage.merge(&usage);
        self.events.push(AgentStateEvent::Usage { usage });

        let is_error = value
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let subtype = value
            .get("subtype")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if is_error {
            self.saw_error = true;
        }
        self.summary.outcome = Some(if is_error {
            IdleOutcome::Failed
        } else {
            IdleOutcome::Succeeded
        });
        self.summary.error = value
            .get("error")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| {
                is_error.then(|| {
                    value
                        .get("result")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            if subtype.is_empty() {
                                "agent error".to_string()
                            } else {
                                subtype.to_string()
                            }
                        })
                })
            });
    }
}

impl StdoutParser for ClaudeStreamJsonParser {
    fn push(&mut self, chunk: &[u8]) {
        ClaudeStreamJsonParser::push(self, chunk);
    }

    fn finish(&mut self, exit_code: Option<i32>) {
        ClaudeStreamJsonParser::finish(self, exit_code);
    }

    fn summary(&self) -> RunSummary {
        self.summary.clone()
    }
}

/// Read a nested string, returning `None` when the pointer is missing.
fn string_at(value: &serde_json::Value, pointer: &str) -> Option<String> {
    value
        .pointer(pointer)
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Read a nested `u64`, returning `0` when the pointer is missing.
fn u64_at(value: &serde_json::Value, pointer: &str) -> u64 {
    value.pointer(pointer).and_then(|v| v.as_u64()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str, exit_code: Option<i32>) -> ClaudeStreamJsonParser {
        let mut parser = ClaudeStreamJsonParser::new();
        parser.push(raw.as_bytes());
        parser.finish(exit_code);
        parser
    }

    const HAPPY: &str = concat!(
        r#"{"type":"system","subtype":"init","session_id":"ses_1","model":"claude-sonnet","capabilities":["x"]}"#,
        "\n",
        r#"{"type":"assistant","session_id":"ses_1","message":{"id":"m1","role":"assistant","content":[{"type":"text","text":"Hello"}]}}"#,
        "\n",
        r#"{"type":"assistant","session_id":"ses_1","message":{"content":[{"type":"tool_use","id":"tu_1","name":"Bash","input":{"command":"ls"}}]}}"#,
        "\n",
        r#"{"type":"user","session_id":"ses_1","message":{"content":[{"type":"tool_result","tool_use_id":"tu_1","content":"a\nb\n","is_error":false}]}}"#,
        "\n",
        r#"{"type":"result","subtype":"success","is_error":false,"result":"Done","session_id":"ses_1","total_cost_usd":0.02,"duration_ms":1234,"num_turns":2,"usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":3,"cache_creation_input_tokens":1}}"#,
        "\n",
    );

    #[test]
    fn happy_path_emits_events_and_summary() {
        let parser = parse(HAPPY, Some(0));
        assert_eq!(
            parser.events(),
            &[
                AgentStateEvent::Session {
                    session_id: Some("ses_1".to_string()),
                    title: None,
                    model: Some("claude-sonnet".to_string()),
                },
                AgentStateEvent::TurnStarted,
                AgentStateEvent::TextDelta {
                    role: MessageRole::Assistant,
                    text: "Hello".to_string(),
                },
                AgentStateEvent::ToolStarted {
                    id: "tu_1".to_string(),
                    name: "Bash".to_string(),
                    input: serde_json::json!({ "command": "ls" }),
                },
                AgentStateEvent::ToolFinished {
                    id: "tu_1".to_string(),
                    name: "Bash".to_string(),
                    ok: true,
                    output: Some(serde_json::json!("a\nb\n")),
                },
                AgentStateEvent::Usage {
                    usage: AgentUsage {
                        input_tokens: 10,
                        output_tokens: 5,
                        reasoning_tokens: 0,
                        cache_read_tokens: 3,
                        cache_write_tokens: 1,
                        cost_usd: Some(0.02),
                    },
                },
                AgentStateEvent::Idle {
                    outcome: IdleOutcome::Succeeded,
                },
            ]
        );

        let summary = parser.summary();
        assert_eq!(summary.session_id.as_deref(), Some("ses_1"));
        assert_eq!(summary.text, "Done");
        assert_eq!(summary.outcome, Some(IdleOutcome::Succeeded));
        assert!(summary.error.is_none());
        assert_eq!(summary.tool_calls.len(), 1);
        assert_eq!(summary.tool_calls[0].name, "Bash");
        assert!(summary.tool_calls[0].ok);
        assert_eq!(
            summary.tool_calls[0].output,
            Some(serde_json::json!("a\nb\n"))
        );
        assert_eq!(summary.usage.input_tokens, 10);
        assert_eq!(summary.usage.cache_read_tokens, 3);
        assert_eq!(summary.usage.cost_usd, Some(0.02));
    }

    #[test]
    fn noise_lines_are_ignored() {
        let noisy = format!(
            "warning: something happened\n\n{}\n{{ not json\n{{\"hello\":\"world\"}}\n{}",
            HAPPY.lines().next().unwrap(),
            HAPPY.lines().skip(1).collect::<Vec<_>>().join("\n")
        );
        let noisy = parse(&noisy, Some(0));
        let clean = parse(HAPPY, Some(0));
        assert_eq!(noisy.events(), clean.events());
        assert_eq!(noisy.summary(), clean.summary());
    }

    #[test]
    fn partial_line_split_across_chunks_is_reassembled() {
        let mut parser = ClaudeStreamJsonParser::new();
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
        let mut parser = ClaudeStreamJsonParser::new();
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
    fn plain_text_leaves_outcome_unset() {
        let parser = parse("plain output\nmore text\n", Some(0));
        assert!(parser.summary().outcome.is_none());
        assert!(parser.summary().text.is_empty());
        assert!(parser.events().is_empty());
    }

    #[test]
    fn result_error_sets_failed_outcome() {
        let raw = concat!(
            r#"{"type":"system","subtype":"init","session_id":"ses_e","model":"claude"}"#,
            "\n",
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"error":"Rate limited","session_id":"ses_e","usage":{"input_tokens":1}}"#,
            "\n",
        );
        let parser = parse(raw, Some(1));
        assert_eq!(parser.summary().outcome, Some(IdleOutcome::Failed));
        assert_eq!(parser.summary().error.as_deref(), Some("Rate limited"));
        let last = parser.events().last().unwrap();
        assert_eq!(
            last,
            &AgentStateEvent::Idle {
                outcome: IdleOutcome::Failed
            }
        );
    }

    #[test]
    fn truncated_stream_without_result_is_interrupted() {
        let raw = concat!(
            r#"{"type":"system","subtype":"init","session_id":"ses_t","model":"claude"}"#,
            "\n",
            r#"{"type":"assistant","session_id":"ses_t","message":{"content":[{"type":"text","text":"partial"}]}}"#,
            "\n",
        );
        let parser = parse(raw, Some(0));
        assert_eq!(parser.summary().outcome, Some(IdleOutcome::Interrupted));
        assert_eq!(parser.summary().text, "partial");
        assert!(parser.summary().error.is_none());
    }

    #[test]
    fn partial_messages_are_not_duplicated_by_assistant_blocks() {
        // With `--include-partial-messages`, the deltas arrive first and the
        // completed assistant block repeats them; the text must be counted once.
        let raw = concat!(
            r#"{"type":"system","subtype":"init","session_id":"ses_p","model":"claude"}"#,
            "\n",
            r#"{"type":"stream_event","session_id":"ses_p","event":{"type":"message_start"}}"#,
            "\n",
            r#"{"type":"stream_event","session_id":"ses_p","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hi"}}}"#,
            "\n",
            r#"{"type":"assistant","session_id":"ses_p","message":{"content":[{"type":"text","text":"Hi there"}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Hi there","session_id":"ses_p"}"#,
            "\n",
        );
        let parser = parse(raw, Some(0));
        let text: String = parser
            .events()
            .iter()
            .filter_map(|e| match e {
                AgentStateEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hi", "the completed block must not be appended twice");
        assert_eq!(parser.summary().text, "Hi there", "the result wins");
        let starts = parser
            .events()
            .iter()
            .filter(|e| **e == AgentStateEvent::TurnStarted)
            .count();
        assert_eq!(starts, 1);
    }

    #[test]
    fn api_retry_is_informational() {
        let raw = concat!(
            r#"{"type":"system","subtype":"init","session_id":"ses_r","model":"claude"}"#,
            "\n",
            r#"{"type":"system","subtype":"api_retry","error":"overloaded","session_id":"ses_r"}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","session_id":"ses_r"}"#,
            "\n",
        );
        let parser = parse(raw, Some(0));
        assert_eq!(parser.summary().outcome, Some(IdleOutcome::Succeeded));
        assert!(parser.summary().error.is_none());
        assert!(parser
            .events()
            .iter()
            .any(|e| matches!(e, AgentStateEvent::Error { message } if message == "overloaded")));
    }

    #[test]
    fn stdout_parser_trait_round_trips() {
        let mut parser = ClaudeStreamJsonParser::new();
        StdoutParser::push(&mut parser, HAPPY.as_bytes());
        StdoutParser::finish(&mut parser, Some(0));
        let summary = StdoutParser::summary(&parser);
        assert_eq!(summary.outcome, Some(IdleOutcome::Succeeded));
    }
}
