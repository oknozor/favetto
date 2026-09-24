//! Tolerant parser for `opencode run --format json` output.
//!
//! opencode writes one JSON object per line and every line carries a top-level
//! `sessionID` and `type`. Unlike the previous all-or-nothing parser, a single
//! malformed line (a progress spinner, a warning, a truncated write) does not
//! discard the whole run: non-JSON lines are skipped, recognized rows are
//! normalized to [`AgentStateEvent`]s, and the result is folded into a
//! [`RunSummary`]. The CLI only emits *completed* tool states and has known
//! truncation bugs, so a missing final step is reported as an interrupted
//! observation rather than quietly assumed to have succeeded.

use favetto_core::model::{
    AgentStateEvent, AgentUsage, IdleOutcome, MessageRole, RunSummary, ToolCall,
};

use crate::agents::agent::{extract_session_id, SessionIdProbe};

/// A streaming, noise-tolerant `opencode-json` parser.
pub(crate) struct OpenCodeJsonlParser {
    /// Bytes seen but not yet terminated by a newline.
    buffer: Vec<u8>,
    probe: Option<SessionIdProbe>,
    events: Vec<AgentStateEvent>,
    summary: RunSummary,
    /// At least one recognized row was seen (so the stream is machine-readable).
    saw_structured: bool,
    /// A final `step_finish` (`reason == "stop"`) marked the turn complete.
    saw_final_step: bool,
    /// An `error` row was seen.
    saw_error: bool,
    finished: bool,
}

impl OpenCodeJsonlParser {
    pub(crate) fn new(probe: Option<SessionIdProbe>) -> Self {
        Self {
            buffer: Vec::new(),
            probe,
            events: Vec::new(),
            summary: RunSummary::default(),
            saw_structured: false,
            saw_final_step: false,
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
    /// `exit_code` is the child's status; it is only used as a tie-breaker,
    /// never to claim success for a stream with no final step.
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
        } else if self.saw_final_step {
            IdleOutcome::Succeeded
        } else {
            // A truncated stream (e.g. opencode dropping the final
            // `step_finish`) must not be reported as a clean success.
            IdleOutcome::Interrupted
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

        match obj.get("type").and_then(|v| v.as_str()).unwrap_or_default() {
            "step_start" => {
                self.saw_structured = true;
                self.events.push(AgentStateEvent::TurnStarted);
            }
            "text" => {
                self.saw_structured = true;
                let text = json_str(&value, "/part/text");
                if !text.is_empty() {
                    self.summary.text.push_str(text);
                    self.events.push(AgentStateEvent::TextDelta {
                        role: MessageRole::Assistant,
                        text: text.to_string(),
                    });
                }
            }
            "reasoning" => {
                self.saw_structured = true;
                let text = json_str(&value, "/part/text");
                if !text.is_empty() {
                    self.summary.reasoning.push_str(text);
                    self.events.push(AgentStateEvent::ReasoningDelta {
                        text: text.to_string(),
                    });
                }
            }
            "tool_use" => {
                self.saw_structured = true;
                self.process_tool_use(obj);
            }
            "step_finish" => {
                self.saw_structured = true;
                self.process_step_finish(obj);
            }
            "error" => {
                self.saw_structured = true;
                let message = json_str(&value, "/error/data/message");
                let message = if message.is_empty() {
                    json_str(&value, "/error/name")
                } else {
                    message
                };
                let message = if message.is_empty() {
                    "agent error"
                } else {
                    message
                };
                self.saw_error = true;
                self.summary.error = Some(message.to_string());
                self.events.push(AgentStateEvent::Error {
                    message: message.to_string(),
                });
            }
            _ => {}
        }
    }

    fn process_tool_use(&mut self, obj: &serde_json::Map<String, serde_json::Value>) {
        let part = obj.get("part");
        let id = part
            .and_then(|p| p.get("callID"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let name = part
            .and_then(|p| p.get("tool"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let state = part.and_then(|p| p.get("state"));
        let status = state
            .and_then(|s| s.get("status"))
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let input = state
            .and_then(|s| s.get("input"))
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let output = state
            .and_then(|s| s.get("output"))
            .cloned()
            .or_else(|| state.and_then(|s| s.pointer("/metadata/output")).cloned())
            .or_else(|| state.and_then(|s| s.get("error")).cloned());
        let ok = status == "completed";

        self.events.push(AgentStateEvent::ToolStarted {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
        });
        self.events.push(AgentStateEvent::ToolFinished {
            id: id.clone(),
            name: name.clone(),
            ok,
            output: output.clone(),
        });
        self.summary.tool_calls.push(ToolCall {
            id,
            name,
            input,
            ok,
            output,
        });
    }

    fn process_step_finish(&mut self, obj: &serde_json::Map<String, serde_json::Value>) {
        let part = obj.get("part");
        if part.and_then(|p| p.get("reason")).and_then(|v| v.as_str()) == Some("stop") {
            self.saw_final_step = true;
        }
        let tokens = part.and_then(|p| p.get("tokens"));
        let usage = AgentUsage {
            input_tokens: u64_at(tokens, "/input"),
            output_tokens: u64_at(tokens, "/output"),
            reasoning_tokens: u64_at(tokens, "/reasoning"),
            cache_read_tokens: u64_at(tokens, "/cache/read"),
            cache_write_tokens: u64_at(tokens, "/cache/write"),
            cost_usd: part.and_then(|p| p.get("cost")).and_then(|v| v.as_f64()),
        };
        self.summary.usage.merge(&usage);
        self.events.push(AgentStateEvent::Usage { usage });
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
fn u64_at(obj: Option<&serde_json::Value>, pointer: &str) -> u64 {
    obj.and_then(|v| v.pointer(pointer))
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parser() -> OpenCodeJsonlParser {
        OpenCodeJsonlParser::new(Some(SessionIdProbe::JsonKey("sessionID".to_string())))
    }

    fn parse(raw: &str, exit_code: Option<i32>) -> OpenCodeJsonlParser {
        let mut parser = parser();
        parser.push(raw.as_bytes());
        parser.finish(exit_code);
        parser
    }

    const HAPPY: &str = concat!(
        r#"{"type":"step_start","timestamp":1,"sessionID":"ses_happy","part":{"type":"step-start"}}"#,
        "\n",
        r#"{"type":"text","timestamp":2,"sessionID":"ses_happy","part":{"type":"text","text":"hello"}}"#,
        "\n",
        r#"{"type":"tool_use","timestamp":3,"sessionID":"ses_happy","part":{"callID":"call_1","tool":"bash","state":{"status":"completed","input":{"command":"echo hi"},"output":"hi\n"}}}"#,
        "\n",
        r#"{"type":"step_finish","timestamp":4,"sessionID":"ses_happy","part":{"type":"step-finish","reason":"stop","cost":0.01,"tokens":{"input":10,"output":5,"reasoning":2,"cache":{"read":3,"write":1}}}}"#,
        "\n",
    );

    #[test]
    fn happy_path_emits_events_and_summary() {
        let parser = parse(HAPPY, Some(0));
        assert_eq!(
            parser.events(),
            &[
                AgentStateEvent::Session {
                    session_id: Some("ses_happy".to_string()),
                    title: None,
                    model: None,
                },
                AgentStateEvent::TurnStarted,
                AgentStateEvent::TextDelta {
                    role: MessageRole::Assistant,
                    text: "hello".to_string(),
                },
                AgentStateEvent::ToolStarted {
                    id: "call_1".to_string(),
                    name: "bash".to_string(),
                    input: serde_json::json!({ "command": "echo hi" }),
                },
                AgentStateEvent::ToolFinished {
                    id: "call_1".to_string(),
                    name: "bash".to_string(),
                    ok: true,
                    output: Some(serde_json::json!("hi\n")),
                },
                AgentStateEvent::Usage {
                    usage: AgentUsage {
                        input_tokens: 10,
                        output_tokens: 5,
                        reasoning_tokens: 2,
                        cache_read_tokens: 3,
                        cache_write_tokens: 1,
                        cost_usd: Some(0.01),
                    },
                },
                AgentStateEvent::Idle {
                    outcome: IdleOutcome::Succeeded,
                },
            ]
        );

        let summary = parser.summary();
        assert_eq!(summary.session_id.as_deref(), Some("ses_happy"));
        assert_eq!(summary.text, "hello");
        assert_eq!(summary.reasoning, "");
        assert_eq!(summary.outcome, Some(IdleOutcome::Succeeded));
        assert!(summary.error.is_none());
        assert_eq!(summary.tool_calls.len(), 1);
        assert_eq!(summary.tool_calls[0].name, "bash");
        assert!(summary.tool_calls[0].ok);
        assert_eq!(summary.usage.input_tokens, 10);
        assert_eq!(summary.usage.cost_usd, Some(0.01));
    }

    #[test]
    fn noise_lines_are_ignored() {
        // Warnings, blank lines, a partial JSON write and an unrelated object
        // are interleaved with the real events; the result is unchanged.
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
    fn truncated_stream_is_interrupted_not_succeeded() {
        // opencode's known truncation bug: `step_start` but no final
        // `step_finish`, even though the process exits 0.
        let raw = concat!(
            r#"{"type":"step_start","timestamp":1,"sessionID":"ses_t","part":{"type":"step-start"}}"#,
            "\n",
            r#"{"type":"text","timestamp":2,"sessionID":"ses_t","part":{"type":"text","text":"partial"}}"#,
            "\n",
        );
        let parser = parse(raw, Some(0));
        assert_eq!(
            parser.events(),
            &[
                AgentStateEvent::Session {
                    session_id: Some("ses_t".to_string()),
                    title: None,
                    model: None,
                },
                AgentStateEvent::TurnStarted,
                AgentStateEvent::TextDelta {
                    role: MessageRole::Assistant,
                    text: "partial".to_string(),
                },
                AgentStateEvent::Idle {
                    outcome: IdleOutcome::Interrupted,
                },
            ]
        );
        let summary = parser.summary();
        assert_eq!(summary.outcome, Some(IdleOutcome::Interrupted));
        assert!(summary.error.is_none());
        assert_eq!(summary.text, "partial");
    }

    #[test]
    fn error_row_sets_failed_outcome() {
        let raw = concat!(
            r#"{"type":"step_start","timestamp":1,"sessionID":"ses_e","part":{"type":"step-start"}}"#,
            "\n",
            r#"{"type":"error","timestamp":2,"sessionID":"ses_e","error":{"name":"APIError","data":{"message":"Rate limit exceeded"}}}"#,
            "\n",
        );
        let parser = parse(raw, Some(1));
        assert_eq!(
            parser.events(),
            &[
                AgentStateEvent::Session {
                    session_id: Some("ses_e".to_string()),
                    title: None,
                    model: None,
                },
                AgentStateEvent::TurnStarted,
                AgentStateEvent::Error {
                    message: "Rate limit exceeded".to_string(),
                },
                AgentStateEvent::Idle {
                    outcome: IdleOutcome::Failed,
                },
            ]
        );
        let summary = parser.summary();
        assert_eq!(summary.outcome, Some(IdleOutcome::Failed));
        assert_eq!(summary.error.as_deref(), Some("Rate limit exceeded"));
    }

    #[test]
    fn unsuccessful_tool_is_recorded() {
        let raw = r#"{"type":"tool_use","sessionID":"ses_x","part":{"callID":"c2","tool":"bash","state":{"status":"error","input":{"command":"false"},"error":"exit 1"}}}"#;
        let parser = parse(raw, Some(0));
        let summary = parser.summary();
        assert_eq!(summary.tool_calls.len(), 1);
        assert!(!summary.tool_calls[0].ok);
        assert_eq!(
            summary.tool_calls[0].output,
            Some(serde_json::json!("exit 1"))
        );
        // Without a final step the run is still an interrupted observation.
        assert_eq!(summary.outcome, Some(IdleOutcome::Interrupted));
    }

    #[test]
    fn plain_text_output_leaves_outcome_unset() {
        // No structured rows at all: keep the old behaviour (exit code decides)
        // and do not invent an outcome.
        let parser = parse("plain output\nmore text\n", Some(0));
        let summary = parser.summary();
        assert!(summary.outcome.is_none());
        assert!(summary.text.is_empty());
        assert!(parser.events().is_empty());
    }

    #[test]
    fn partial_line_split_across_chunks_is_reassembled() {
        let mut parser = parser();
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
    fn session_id_is_announced_once() {
        let parser = parse(HAPPY, Some(0));
        let sessions = parser
            .events()
            .iter()
            .filter(|e| matches!(e, AgentStateEvent::Session { .. }))
            .count();
        assert_eq!(sessions, 1);
    }

    #[test]
    fn finish_is_idempotent() {
        let mut parser = parser();
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
}
