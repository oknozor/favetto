//! The `opencode-server` [`StateSource`]: one SSE subscription per session,
//! reconciled against REST, mapped into the normalized [`AgentStateEvent`]
//! vocabulary.
//!
//! The design table in `docs/design/agent-state-adapters.md` §7.1 names the
//! opencode v2.0.8 events (`message.updated`, `message.part.updated`,
//! `session.status`). The installed CLI (v2.0.14) uses a richer vocabulary
//! (`session.execution.*`, `session.text/reasoning/tool/usage.*`,
//! `session.renamed`), so the mapper treats those as primary and keeps the
//! design names as aliases. SSE is best-effort by opencode's own contract, so a
//! periodic tick reconciles pending permissions and the session outcome from
//! REST.
//!
//! See `.favetto/plans/159/plan.md`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc;

use favetto_core::model::{
    AgentStateEvent, AgentUsage, AwaitingInputKind, IdleOutcome, InputReply, InputRequest,
    MessageRole,
};

use crate::agents::state::{InputResponder, StateContext, StateSource, StateStart};

use super::server::{self, Endpoint};

/// The opencode live-state transport.
pub(crate) struct OpenCodeServer {
    endpoint: Endpoint,
}

impl OpenCodeServer {
    pub(crate) fn new(endpoint: Endpoint) -> Self {
        Self { endpoint }
    }
}

impl StateSource for OpenCodeServer {
    fn label(&self) -> &'static str {
        "opencode-server"
    }

    fn start(&self, ctx: StateContext) -> anyhow::Result<StateStart> {
        let session = ctx
            .external_session
            .clone()
            .ok_or_else(|| anyhow::anyhow!("managed opencode observer requires a session id"))?;
        let (tx, events) = mpsc::unbounded_channel();
        // Seed the identity so `AgentManager` records the id before any SSE
        // frame arrives (the interactive TUI emits no JSON `sessionID`).
        let _ = tx.send(AgentStateEvent::Session {
            session_id: Some(session.clone()),
            title: None,
            model: None,
        });
        let task = tokio::spawn(run_observer(self.endpoint.clone(), session.clone(), tx));
        let abort = task.abort_handle();
        Ok(StateStart {
            events,
            stdout: None,
            responder: Arc::new(OpenCodeResponder {
                endpoint: self.endpoint.clone(),
                session_id: session,
            }),
            stop: Some(Box::new(move || abort.abort())),
        })
    }
}

/// Answers permission prompts over `POST …/permission/{requestID}/reply`.
struct OpenCodeResponder {
    endpoint: Endpoint,
    session_id: String,
}

impl InputResponder for OpenCodeResponder {
    fn reply<'a>(
        &'a self,
        request_id: &'a str,
        reply: InputReply,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let decision = decision_for(&reply).ok_or_else(|| {
                anyhow::anyhow!("opencode only supports once/always/reject replies")
            })?;
            let path = format!(
                "/api/session/{}/permission/{}/reply",
                server::encode_segment(&self.session_id),
                server::encode_segment(request_id)
            );
            let resp = self
                .endpoint
                .request(reqwest::Method::POST, &path)
                .json(&serde_json::json!({ "decision": decision }))
                .send()
                .await?;
            if !resp.status().is_success() {
                anyhow::bail!("opencode permission reply failed ({})", resp.status());
            }
            Ok(())
        })
    }
}

/// The permission decision opencode expects for a typed reply, if any.
pub(crate) fn decision_for(reply: &InputReply) -> Option<&'static str> {
    match reply {
        InputReply::Once => Some("once"),
        InputReply::Always => Some("always"),
        InputReply::Reject => Some("reject"),
        InputReply::Value { .. } | InputReply::Confirmed { .. } | InputReply::Cancelled => None,
    }
}

/// Parse one SSE line, returning the JSON object after a `data:` prefix.
///
/// Comment (`: heartbeat`), `event:` and blank lines all return `None`.
pub(crate) fn parse_sse_data(line: &str) -> Option<Value> {
    let payload = line.trim().strip_prefix("data:")?.trim();
    if payload.is_empty() {
        return None;
    }
    serde_json::from_str(payload).ok()
}

/// Map one event with no cross-event state (see [`Mapper`] for the stateful one).
#[cfg(test)]
pub(crate) fn map_event(value: &Value, session_id: &str) -> Vec<AgentStateEvent> {
    Mapper::default().map(value, session_id)
}

/// Stateful event mapper: remembers tool names and pending permissions across a
/// stream so later events that only carry a call id can still name the tool.
#[derive(Default)]
struct Mapper {
    names: HashMap<String, String>,
    outstanding: HashSet<String>,
    last_title: Option<String>,
}

impl Mapper {
    fn map(&mut self, value: &Value, session_id: &str) -> Vec<AgentStateEvent> {
        // Scope every event to this session; global events (and other sessions'
        // traffic on the shared daemon-wide server) are ignored.
        match event_session(value) {
            Some(sid) if sid == session_id => {}
            _ => return Vec::new(),
        }
        match event_type(value) {
            "session.created" => {
                let mut events = vec![AgentStateEvent::Session {
                    session_id: Some(session_id.to_string()),
                    title: None,
                    model: None,
                }];
                events.extend(self.title_event(data_of(value)));
                events
            }
            "session.renamed" | "session.updated" => self.title_event(data_of(value)),
            "session.execution.started" | "session.step.started" | "message.updated" => {
                self.turn_started(value)
            }
            "session.text.delta" => text_delta(data_of(value)),
            "session.reasoning.delta" => reasoning_delta(data_of(value)),
            "session.tool.input.started" => self.tool_started(data_of(value)),
            "session.tool.input.ended" => self.tool_input_ended(data_of(value)),
            "session.tool.called" => tool_updated(data_of(value), "/input"),
            "session.tool.progress" => tool_updated(data_of(value), "/metadata"),
            "session.tool.success" => self.tool_finished(data_of(value), true),
            "session.tool.failed" => self.tool_finished(data_of(value), false),
            "session.step.ended" | "session.usage.updated" | "session.usage.recorded" => {
                usage_event(data_of(value))
            }
            "session.execution.succeeded" => vec![AgentStateEvent::Idle {
                outcome: IdleOutcome::Succeeded,
            }],
            "session.execution.interrupted" => vec![AgentStateEvent::Idle {
                outcome: IdleOutcome::Interrupted,
            }],
            "session.execution.failed" => execution_failed(data_of(value)),
            "session.step.failed" | "session.error" => vec![AgentStateEvent::Error {
                message: error_message(data_of(value)),
            }],
            "permission.asked" => self.permission_event(data_of(value)).into_iter().collect(),
            "permission.replied" | "permission.rejected" => {
                self.permission_resolved(data_of(value))
            }
            // Legacy v2.0.8 aliases.
            "message.part.updated" => self.legacy_part(value),
            "session.status" => legacy_status(value),
            "session.idle" => vec![AgentStateEvent::Idle {
                outcome: IdleOutcome::Succeeded,
            }],
            _ => Vec::new(),
        }
    }

    fn title_event(&mut self, data: &Value) -> Vec<AgentStateEvent> {
        let Some(title) = data.get("title").and_then(Value::as_str) else {
            return Vec::new();
        };
        let title = title.trim();
        if title.is_empty() || self.last_title.as_deref() == Some(title) {
            return Vec::new();
        }
        self.last_title = Some(title.to_string());
        vec![AgentStateEvent::Title {
            title: title.to_string(),
        }]
    }

    fn turn_started(&mut self, value: &Value) -> Vec<AgentStateEvent> {
        // The legacy `message.updated` only marks a turn for the assistant role.
        if event_type(value) == "message.updated" {
            let role = value
                .pointer("/properties/info/role")
                .or_else(|| value.pointer("/data/role"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if role != "assistant" {
                return Vec::new();
            }
        }
        vec![AgentStateEvent::TurnStarted]
    }

    fn tool_started(&mut self, data: &Value) -> Vec<AgentStateEvent> {
        let Some(id) = data.get("id").and_then(Value::as_str) else {
            return Vec::new();
        };
        let name = data
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        self.names.insert(id.to_string(), name.clone());
        vec![AgentStateEvent::ToolStarted {
            id: id.to_string(),
            name,
            input: Value::Null,
        }]
    }

    fn tool_input_ended(&mut self, data: &Value) -> Vec<AgentStateEvent> {
        let Some(id) = data.get("id").and_then(Value::as_str) else {
            return Vec::new();
        };
        let partial = data
            .get("text")
            .and_then(Value::as_str)
            .and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or(Value::Null);
        self.tool_updated_event(id, partial)
    }

    fn tool_finished(&mut self, data: &Value, ok: bool) -> Vec<AgentStateEvent> {
        let Some(id) = data.get("id").and_then(Value::as_str) else {
            return Vec::new();
        };
        let name = self
            .names
            .get(id)
            .cloned()
            .or_else(|| data.get("name").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default();
        let output = data
            .get("content")
            .cloned()
            .or_else(|| data.get("error").cloned())
            .or_else(|| data.get("output").cloned());
        vec![AgentStateEvent::ToolFinished {
            id: id.to_string(),
            name,
            ok,
            output,
        }]
    }

    fn tool_updated_event(&self, id: &str, partial: Value) -> Vec<AgentStateEvent> {
        vec![AgentStateEvent::ToolUpdated {
            id: id.to_string(),
            partial,
        }]
    }

    /// Map a native or REST [`Permission.Request`](opencode) into an
    /// `InputRequested`, deduplicating against already-outstanding ids.
    fn permission_event(&mut self, data: &Value) -> Option<AgentStateEvent> {
        let id = data.get("id").and_then(Value::as_str)?;
        if !self.outstanding.insert(id.to_string()) {
            return None;
        }
        let action = data
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let resources = data
            .get("resources")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let message = format!("Allow {action} {resources}").trim().to_string();
        let allow_always = data
            .get("save")
            .and_then(Value::as_array)
            .map(|saved| !saved.is_empty())
            .unwrap_or(false);
        Some(AgentStateEvent::InputRequested {
            request: InputRequest {
                id: id.to_string(),
                kind: AwaitingInputKind::Permission,
                message,
                options: vec![
                    "Allow once".to_string(),
                    "Allow always".to_string(),
                    "Reject".to_string(),
                ],
                allow_always,
            },
        })
    }

    fn permission_resolved(&mut self, data: &Value) -> Vec<AgentStateEvent> {
        let id = data
            .get("requestID")
            .or_else(|| data.get("id"))
            .and_then(Value::as_str);
        match id {
            Some(id) => {
                self.outstanding.remove(id);
                vec![AgentStateEvent::InputResolved { id: id.to_string() }]
            }
            None => Vec::new(),
        }
    }

    /// Legacy `message.part.updated` (v2.0.8): text, reasoning, tool, step.
    fn legacy_part(&mut self, value: &Value) -> Vec<AgentStateEvent> {
        let part = value
            .pointer("/properties/part")
            .or_else(|| value.pointer("/data/part"))
            .or_else(|| value.pointer("/part"));
        let part_type = part
            .and_then(|p| p.get("type"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        match part_type {
            "text" => text_delta(value),
            "reasoning" => reasoning_delta(value),
            "step-start" => vec![AgentStateEvent::TurnStarted],
            "step-finish" => {
                let usage = usage_from(value.pointer("/properties/part").unwrap_or(value));
                usage_event_from(usage)
            }
            "tool" => {
                let part = part.unwrap_or(value);
                self.legacy_tool(part)
            }
            _ => Vec::new(),
        }
    }

    fn legacy_tool(&mut self, part: &Value) -> Vec<AgentStateEvent> {
        let id = part
            .get("callID")
            .or_else(|| part.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let name = part
            .get("tool")
            .or_else(|| part.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !name.is_empty() {
            self.names.insert(id.clone(), name.clone());
        }
        let state = part.get("state");
        let status = state
            .and_then(|s| s.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("running");
        match status {
            "completed" | "error" => {
                let output = state
                    .and_then(|s| s.get("output"))
                    .cloned()
                    .or_else(|| state.and_then(|s| s.get("error")).cloned());
                vec![AgentStateEvent::ToolFinished {
                    id,
                    name,
                    ok: status == "completed",
                    output,
                }]
            }
            _ => vec![AgentStateEvent::ToolStarted {
                id,
                name,
                input: state
                    .and_then(|s| s.get("input"))
                    .cloned()
                    .unwrap_or(Value::Null),
            }],
        }
    }
}

fn event_type(value: &Value) -> &str {
    value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn data_of(value: &Value) -> &Value {
    value.get("data").unwrap_or(&Value::Null)
}

/// The session an event belongs to, across both the v2.0.14 (`data.sessionID`)
/// and design (`sessionID` / `properties.sessionID`) shapes.
fn event_session(value: &Value) -> Option<&str> {
    [
        value.pointer("/data/sessionID"),
        value.pointer("/data/part/sessionID"),
        value.get("sessionID"),
        value.pointer("/properties/sessionID"),
        value.pointer("/properties/part/sessionID"),
    ]
    .into_iter()
    .flatten()
    .find_map(Value::as_str)
}

fn text_delta(value: &Value) -> Vec<AgentStateEvent> {
    let text = value
        .get("delta")
        .and_then(Value::as_str)
        .or_else(|| value.get("text").and_then(Value::as_str))
        .or_else(|| value.pointer("/properties/delta").and_then(Value::as_str))
        .or_else(|| {
            value
                .pointer("/properties/part/text")
                .and_then(Value::as_str)
        })
        .or_else(|| value.pointer("/part/text").and_then(Value::as_str))
        .unwrap_or_default();
    if text.is_empty() {
        return Vec::new();
    }
    vec![AgentStateEvent::TextDelta {
        role: MessageRole::Assistant,
        text: text.to_string(),
    }]
}

fn reasoning_delta(value: &Value) -> Vec<AgentStateEvent> {
    let text = value
        .get("delta")
        .and_then(Value::as_str)
        .or_else(|| value.get("text").and_then(Value::as_str))
        .or_else(|| value.pointer("/properties/delta").and_then(Value::as_str))
        .or_else(|| {
            value
                .pointer("/properties/part/text")
                .and_then(Value::as_str)
        })
        .or_else(|| value.pointer("/part/text").and_then(Value::as_str))
        .unwrap_or_default();
    if text.is_empty() {
        return Vec::new();
    }
    vec![AgentStateEvent::ReasoningDelta {
        text: text.to_string(),
    }]
}

fn tool_updated(data: &Value, pointer: &str) -> Vec<AgentStateEvent> {
    let Some(id) = data.get("id").and_then(Value::as_str) else {
        return Vec::new();
    };
    vec![AgentStateEvent::ToolUpdated {
        id: id.to_string(),
        partial: data.pointer(pointer).cloned().unwrap_or(Value::Null),
    }]
}

fn usage_event(data: &Value) -> Vec<AgentStateEvent> {
    usage_event_from(usage_from(data))
}

/// Build a `Usage` event, or nothing when the payload carried no counters.
fn usage_event_from(usage: AgentUsage) -> Vec<AgentStateEvent> {
    if usage.is_empty() {
        return Vec::new();
    }
    vec![AgentStateEvent::Usage { usage }]
}

fn usage_from(value: &Value) -> AgentUsage {
    let tokens = value.get("tokens").unwrap_or(&Value::Null);
    AgentUsage {
        input_tokens: tokens.get("input").and_then(Value::as_u64).unwrap_or(0),
        output_tokens: tokens.get("output").and_then(Value::as_u64).unwrap_or(0),
        reasoning_tokens: tokens.get("reasoning").and_then(Value::as_u64).unwrap_or(0),
        cache_read_tokens: tokens
            .pointer("/cache/read")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_write_tokens: tokens
            .pointer("/cache/write")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cost_usd: value.get("cost").and_then(Value::as_f64),
    }
}

fn execution_failed(data: &Value) -> Vec<AgentStateEvent> {
    vec![
        AgentStateEvent::Error {
            message: error_message(data),
        },
        AgentStateEvent::Idle {
            outcome: IdleOutcome::Failed,
        },
    ]
}

fn error_message(data: &Value) -> String {
    data.pointer("/error/message")
        .or_else(|| data.pointer("/error/type"))
        .or_else(|| data.get("message"))
        .and_then(Value::as_str)
        .filter(|m| !m.trim().is_empty())
        .unwrap_or("opencode error")
        .to_string()
}

fn legacy_status(value: &Value) -> Vec<AgentStateEvent> {
    let status = value
        .pointer("/properties/status/type")
        .or_else(|| value.pointer("/data/status/type"))
        .or_else(|| value.pointer("/properties/status"))
        .or_else(|| value.pointer("/data/status"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if status == "idle" {
        vec![AgentStateEvent::Idle {
            outcome: IdleOutcome::Succeeded,
        }]
    } else {
        Vec::new()
    }
}

/// Stream `/api/event` until a terminal event (or the stream ends), returning
/// whether the session reached a terminal state.
async fn observe_stream(
    ep: &Endpoint,
    session: &str,
    tx: &mpsc::UnboundedSender<AgentStateEvent>,
    mapper: &mut Mapper,
) -> bool {
    let resp = match ep.request(reqwest::Method::GET, "/api/event").send().await {
        Ok(resp) if resp.status().is_success() => resp,
        _ => return false,
    };
    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            chunk = stream.next() => match chunk {
                Some(Ok(bytes)) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes));
                    while let Some(pos) = buffer.find('\n') {
                        let line = buffer[..pos].trim_end_matches('\r').to_string();
                        buffer.drain(..=pos);
                        let Some(value) = parse_sse_data(&line) else { continue };
                        for event in mapper.map(&value, session) {
                            let terminal = matches!(event, AgentStateEvent::Idle { .. });
                            if tx.send(event).is_err() {
                                return true;
                            }
                            if terminal {
                                return true;
                            }
                        }
                    }
                }
                _ => return false,
            },
            _ = tick.tick() => {
                if reconcile(ep, session, tx, mapper).await {
                    return true;
                }
            }
        }
    }
}

/// Re-emit pending permissions and the session outcome from REST. Returns
/// `true` when the session reached a terminal outcome.
async fn reconcile(
    ep: &Endpoint,
    session: &str,
    tx: &mpsc::UnboundedSender<AgentStateEvent>,
    mapper: &mut Mapper,
) -> bool {
    let permission_path = format!(
        "/api/session/{}/permission",
        server::encode_segment(session)
    );
    if let Ok(resp) = ep
        .request(reqwest::Method::GET, &permission_path)
        .send()
        .await
    {
        if resp.status().is_success() {
            if let Ok(value) = resp.json::<Value>().await {
                if let Some(items) = value.get("data").and_then(Value::as_array) {
                    for item in items {
                        if let Some(event) = mapper.permission_event(item) {
                            if tx.send(event).is_err() {
                                return true;
                            }
                        }
                    }
                }
            }
        }
    }

    let session_path = format!("/api/session/{}", server::encode_segment(session));
    if let Ok(resp) = ep.request(reqwest::Method::GET, &session_path).send().await {
        if resp.status().is_success() {
            if let Ok(value) = resp.json::<Value>().await {
                let data = data_of(&value);
                for event in mapper.title_event(data) {
                    if tx.send(event).is_err() {
                        return true;
                    }
                }
                if let Some(outcome) = data.get("outcome").and_then(Value::as_str) {
                    let outcome = match outcome {
                        "succeeded" => IdleOutcome::Succeeded,
                        "failed" => IdleOutcome::Failed,
                        "interrupted" => IdleOutcome::Interrupted,
                        _ => return false,
                    };
                    let _ = tx.send(AgentStateEvent::Idle { outcome });
                    return true;
                }
            }
        }
    }
    false
}

/// Reconnect until the stream reports a terminal event or the budget runs out.
/// Re-reads [`server::endpoint`] so a supervised restart on a new port is seen.
async fn run_observer(
    initial: Endpoint,
    session: String,
    tx: mpsc::UnboundedSender<AgentStateEvent>,
) {
    const MAX_ATTEMPTS: u32 = 10;
    let mut mapper = Mapper::default();
    for attempt in 0..=MAX_ATTEMPTS {
        let ep = server::endpoint().unwrap_or_else(|| initial.clone());
        if observe_stream(&ep, &session, &tx, &mut mapper).await {
            return;
        }
        if tx.is_closed() {
            return;
        }
        if attempt < MAX_ATTEMPTS {
            tokio::time::sleep(server::backoff(attempt)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::Arc;

    use axum::body::Body;
    use axum::extract::{Path, State};
    use axum::routing::{get, post};
    use axum::{Json, Router};

    fn test_context(session: Option<&str>) -> StateContext {
        StateContext {
            favetto_session: "sess_local".to_string(),
            external_session: session.map(str::to_string),
            prompt: None,
            headless: false,
            cwd: None,
            program: PathBuf::from("opencode"),
            args: Vec::new(),
            env: Default::default(),
            stdin: Arc::new(parking_lot::Mutex::new(
                Box::new(Vec::<u8>::new()) as Box<dyn Write + Send>
            )),
        }
    }

    fn event(json: &str) -> Value {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn maps_session_created_and_title() {
        let value = event(
            r#"{"type":"session.created","data":{"sessionID":"ses_1","title":"Fix the widget"}}"#,
        );
        assert_eq!(
            map_event(&value, "ses_1"),
            vec![
                AgentStateEvent::Session {
                    session_id: Some("ses_1".to_string()),
                    title: None,
                    model: None,
                },
                AgentStateEvent::Title {
                    title: "Fix the widget".to_string(),
                },
            ]
        );
        // A different session is out of scope.
        assert!(map_event(&value, "ses_2").is_empty());
        // A global event is out of scope.
        assert!(map_event(&event(r#"{"type":"server.connected","data":{}}"#), "ses_1").is_empty());
    }

    #[test]
    fn maps_renamed_to_title() {
        let value =
            event(r#"{"type":"session.renamed","data":{"sessionID":"ses_1","title":"Renamed"}}"#);
        assert_eq!(
            map_event(&value, "ses_1"),
            vec![AgentStateEvent::Title {
                title: "Renamed".to_string()
            }]
        );
    }

    #[test]
    fn maps_turn_text_and_reasoning() {
        assert_eq!(
            map_event(
                &event(r#"{"type":"session.execution.started","data":{"sessionID":"ses_1"}}"#),
                "ses_1"
            ),
            vec![AgentStateEvent::TurnStarted]
        );
        assert_eq!(
            map_event(
                &event(
                    r#"{"type":"session.step.started","data":{"sessionID":"ses_1","model":{"id":"m","providerID":"p"}}}"#
                ),
                "ses_1"
            ),
            vec![AgentStateEvent::TurnStarted]
        );
        assert_eq!(
            map_event(
                &event(
                    r#"{"type":"session.text.delta","data":{"sessionID":"ses_1","delta":"hello"}}"#
                ),
                "ses_1"
            ),
            vec![AgentStateEvent::TextDelta {
                role: MessageRole::Assistant,
                text: "hello".to_string()
            }]
        );
        assert_eq!(
            map_event(
                &event(
                    r#"{"type":"session.reasoning.delta","data":{"sessionID":"ses_1","delta":"why"}}"#
                ),
                "ses_1"
            ),
            vec![AgentStateEvent::ReasoningDelta {
                text: "why".to_string()
            }]
        );
    }

    #[test]
    fn maps_tool_lifecycle_with_name_recovery() {
        let mut mapper = Mapper::default();
        let started = mapper.map(
            &event(r#"{"type":"session.tool.input.started","data":{"sessionID":"ses_1","id":"call_1","name":"shell"}}"#),
            "ses_1",
        );
        assert_eq!(
            started,
            vec![AgentStateEvent::ToolStarted {
                id: "call_1".to_string(),
                name: "shell".to_string(),
                input: Value::Null,
            }]
        );

        let ended = mapper.map(
            &event(r#"{"type":"session.tool.input.ended","data":{"sessionID":"ses_1","id":"call_1","text":"{\"command\":\"echo hi\"}"}}"#),
            "ses_1",
        );
        assert_eq!(
            ended,
            vec![AgentStateEvent::ToolUpdated {
                id: "call_1".to_string(),
                partial: serde_json::json!({ "command": "echo hi" }),
            }]
        );

        let called = mapper.map(
            &event(r#"{"type":"session.tool.called","data":{"sessionID":"ses_1","id":"call_1","input":{"command":"echo hi"}}}"#),
            "ses_1",
        );
        assert_eq!(
            called,
            vec![AgentStateEvent::ToolUpdated {
                id: "call_1".to_string(),
                partial: serde_json::json!({ "command": "echo hi" }),
            }]
        );

        let progress = mapper.map(
            &event(r#"{"type":"session.tool.progress","data":{"sessionID":"ses_1","id":"call_1","metadata":{"shellID":"sh_1"}}}"#),
            "ses_1",
        );
        assert_eq!(
            progress,
            vec![AgentStateEvent::ToolUpdated {
                id: "call_1".to_string(),
                partial: serde_json::json!({ "shellID": "sh_1" }),
            }]
        );

        // `success` carries no name; it is recovered from the earlier start.
        let success = mapper.map(
            &event(r#"{"type":"session.tool.success","data":{"sessionID":"ses_1","id":"call_1","content":[{"type":"text","text":"hi"}]}}"#),
            "ses_1",
        );
        assert_eq!(
            success,
            vec![AgentStateEvent::ToolFinished {
                id: "call_1".to_string(),
                name: "shell".to_string(),
                ok: true,
                output: Some(serde_json::json!([{ "type": "text", "text": "hi" }])),
            }]
        );

        let failed = map_event(
            &event(
                r#"{"type":"session.tool.failed","data":{"sessionID":"ses_1","id":"call_2","name":"bash","error":"boom"}}"#,
            ),
            "ses_1",
        );
        assert_eq!(
            failed,
            vec![AgentStateEvent::ToolFinished {
                id: "call_2".to_string(),
                name: "bash".to_string(),
                ok: false,
                output: Some(serde_json::json!("boom")),
            }]
        );
    }

    #[test]
    fn maps_usage_from_step_and_usage_events() {
        let step = r#"{"type":"session.step.ended","data":{"sessionID":"ses_1","cost":0.01,"tokens":{"input":10,"output":5,"reasoning":2,"cache":{"read":3,"write":1}}}}"#;
        let expected = AgentUsage {
            input_tokens: 10,
            output_tokens: 5,
            reasoning_tokens: 2,
            cache_read_tokens: 3,
            cache_write_tokens: 1,
            cost_usd: Some(0.01),
        };
        assert_eq!(
            map_event(&event(step), "ses_1"),
            vec![AgentStateEvent::Usage { usage: expected }]
        );

        let usage = r#"{"type":"session.usage.updated","data":{"sessionID":"ses_1","cost":0.02,"tokens":{"input":1}}}"#;
        assert_eq!(
            map_event(&event(usage), "ses_1"),
            vec![AgentStateEvent::Usage {
                usage: AgentUsage {
                    input_tokens: 1,
                    cost_usd: Some(0.02),
                    ..Default::default()
                }
            }]
        );
    }

    #[test]
    fn maps_terminal_execution_events() {
        assert_eq!(
            map_event(
                &event(r#"{"type":"session.execution.succeeded","data":{"sessionID":"ses_1"}}"#),
                "ses_1"
            ),
            vec![AgentStateEvent::Idle {
                outcome: IdleOutcome::Succeeded
            }]
        );
        assert_eq!(
            map_event(
                &event(r#"{"type":"session.execution.interrupted","data":{"sessionID":"ses_1"}}"#),
                "ses_1"
            ),
            vec![AgentStateEvent::Idle {
                outcome: IdleOutcome::Interrupted
            }]
        );
        assert_eq!(
            map_event(
                &event(
                    r#"{"type":"session.execution.failed","data":{"sessionID":"ses_1","error":{"type":"APIError","message":"Rate limited"}}}"#
                ),
                "ses_1"
            ),
            vec![
                AgentStateEvent::Error {
                    message: "Rate limited".to_string()
                },
                AgentStateEvent::Idle {
                    outcome: IdleOutcome::Failed
                },
            ]
        );
    }

    #[test]
    fn maps_permission_asked_and_resolved() {
        let asked = event(
            r#"{"type":"permission.asked","data":{"id":"per_1","sessionID":"ses_1","action":"bash","resources":["echo hi"],"save":["always"]}}"#,
        );
        assert_eq!(
            map_event(&asked, "ses_1"),
            vec![AgentStateEvent::InputRequested {
                request: InputRequest {
                    id: "per_1".to_string(),
                    kind: AwaitingInputKind::Permission,
                    message: "Allow bash echo hi".to_string(),
                    options: vec![
                        "Allow once".to_string(),
                        "Allow always".to_string(),
                        "Reject".to_string()
                    ],
                    allow_always: true,
                }
            }]
        );

        // No `save` array means "always" is not offered.
        let no_save = event(
            r#"{"type":"permission.asked","data":{"id":"per_2","sessionID":"ses_1","action":"read","resources":[]}}"#,
        );
        let mapped = map_event(&no_save, "ses_1");
        match &mapped[0] {
            AgentStateEvent::InputRequested { request } => {
                assert!(!request.allow_always);
                assert_eq!(request.message, "Allow read");
            }
            other => panic!("unexpected: {other:?}"),
        }

        assert_eq!(
            map_event(
                &event(
                    r#"{"type":"permission.replied","data":{"sessionID":"ses_1","requestID":"per_1","reply":"always"}}"#
                ),
                "ses_1"
            ),
            vec![AgentStateEvent::InputResolved {
                id: "per_1".to_string()
            }]
        );
        assert_eq!(
            map_event(
                &event(
                    r#"{"type":"permission.rejected","data":{"sessionID":"ses_1","requestID":"per_2"}}"#
                ),
                "ses_1"
            ),
            vec![AgentStateEvent::InputResolved {
                id: "per_2".to_string()
            }]
        );
    }

    #[test]
    fn permission_dedupe_across_sse_and_reconcile() {
        let mut mapper = Mapper::default();
        let asked = event(
            r#"{"type":"permission.asked","data":{"id":"per_1","sessionID":"ses_1","action":"bash","resources":[]}}"#,
        );
        assert_eq!(mapper.map(&asked, "ses_1").len(), 1);
        // A second observation of the same pending request emits nothing.
        assert!(mapper.map(&asked, "ses_1").is_empty());
    }

    #[test]
    fn maps_legacy_design_events() {
        // v2.0.8 text part.
        assert_eq!(
            map_event(
                &event(
                    r#"{"type":"message.part.updated","sessionID":"ses_1","properties":{"part":{"type":"text","text":"hi"}}}"#
                ),
                "ses_1"
            ),
            vec![AgentStateEvent::TextDelta {
                role: MessageRole::Assistant,
                text: "hi".to_string()
            }]
        );
        // v2.0.8 step-start.
        assert_eq!(
            map_event(
                &event(
                    r#"{"type":"message.part.updated","sessionID":"ses_1","properties":{"part":{"type":"step-start"}}}"#
                ),
                "ses_1"
            ),
            vec![AgentStateEvent::TurnStarted]
        );
        // v2.0.8 idle status.
        assert_eq!(
            map_event(
                &event(
                    r#"{"type":"session.status","sessionID":"ses_1","properties":{"status":{"type":"idle"}}}"#
                ),
                "ses_1"
            ),
            vec![AgentStateEvent::Idle {
                outcome: IdleOutcome::Succeeded
            }]
        );
        // v2.0.8 assistant message.
        assert_eq!(
            map_event(
                &event(
                    r#"{"type":"message.updated","sessionID":"ses_1","properties":{"info":{"role":"assistant"}}}"#
                ),
                "ses_1"
            ),
            vec![AgentStateEvent::TurnStarted]
        );
    }

    #[test]
    fn decision_for_supports_only_permission_answers() {
        assert_eq!(decision_for(&InputReply::Once), Some("once"));
        assert_eq!(decision_for(&InputReply::Always), Some("always"));
        assert_eq!(decision_for(&InputReply::Reject), Some("reject"));
        assert!(decision_for(&InputReply::Value {
            value: "x".to_string()
        })
        .is_none());
        assert!(decision_for(&InputReply::Confirmed { confirmed: true }).is_none());
        assert!(decision_for(&InputReply::Cancelled).is_none());
    }

    #[test]
    fn parse_sse_data_handles_framing() {
        assert!(parse_sse_data(": heartbeat").is_none());
        assert!(parse_sse_data("event: message").is_none());
        assert!(parse_sse_data("").is_none());
        assert!(parse_sse_data("data: { broken").is_none());
        let value = parse_sse_data("data: {\"type\":\"x\"}").unwrap();
        assert_eq!(value["type"], "x");
        // A line split across chunks is reassembled by the caller, so parsing a
        // complete `data:` line is all this helper needs to do.
        assert_eq!(parse_sse_data("data:{\"a\":1}").unwrap()["a"], 1);
    }

    // ---- Mock server end-to-end ----

    const SSE_BODY: &str = concat!(
        "data: {\"type\":\"session.created\",\"data\":{\"sessionID\":\"ses_test\",\"title\":\"Probe\"}}\n\n",
        ": heartbeat\n\n",
        "data: {\"type\":\"session.execution.started\",\"data\":{\"sessionID\":\"ses_test\"}}\n\n",
        "data: {\"type\":\"session.text.delta\",\"data\":{\"sessionID\":\"ses_test\",\"delta\":\"hello\"}}\n\n",
        "data: {\"type\":\"session.tool.input.started\",\"data\":{\"sessionID\":\"ses_test\",\"id\":\"call_1\",\"name\":\"shell\"}}\n\n",
        "data: {\"type\":\"session.tool.success\",\"data\":{\"sessionID\":\"ses_test\",\"id\":\"call_1\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n\n",
        "data: {\"type\":\"session.step.ended\",\"data\":{\"sessionID\":\"ses_test\",\"cost\":0.01,\"tokens\":{\"input\":10,\"output\":5,\"reasoning\":2,\"cache\":{\"read\":3,\"write\":1}}}}\n\n",
        "data: {\"type\":\"session.execution.succeeded\",\"data\":{\"sessionID\":\"ses_test\"}}\n\n",
    );

    async fn sse_handler() -> axum::response::Response {
        axum::response::Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from(SSE_BODY))
            .unwrap()
    }

    async fn spawn_mock(app: Router) -> Endpoint {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Endpoint {
            url: format!("http://{addr}"),
            password: "test".to_string(),
        }
    }

    #[tokio::test]
    async fn observer_streams_and_maps_a_scripted_server() {
        // A stale endpoint from another test on this thread must not shadow the
        // mock server passed to `OpenCodeServer::new`.
        server::install_endpoint_for_test(None);
        let app = Router::new().route("/api/event", get(sse_handler));
        let ep = spawn_mock(app).await;
        let start = OpenCodeServer::new(ep)
            .start(test_context(Some("ses_test")))
            .unwrap();
        let mut events = start.events;
        let mut collected = Vec::new();
        while let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_secs(5), events.recv()).await
        {
            collected.push(event);
        }
        assert_eq!(
            collected,
            vec![
                AgentStateEvent::Session {
                    session_id: Some("ses_test".to_string()),
                    title: None,
                    model: None,
                },
                AgentStateEvent::Session {
                    session_id: Some("ses_test".to_string()),
                    title: None,
                    model: None,
                },
                AgentStateEvent::Title {
                    title: "Probe".to_string()
                },
                AgentStateEvent::TurnStarted,
                AgentStateEvent::TextDelta {
                    role: MessageRole::Assistant,
                    text: "hello".to_string()
                },
                AgentStateEvent::ToolStarted {
                    id: "call_1".to_string(),
                    name: "shell".to_string(),
                    input: Value::Null,
                },
                AgentStateEvent::ToolFinished {
                    id: "call_1".to_string(),
                    name: "shell".to_string(),
                    ok: true,
                    output: Some(serde_json::json!([{ "type": "text", "text": "hi" }])),
                },
                AgentStateEvent::Usage {
                    usage: AgentUsage {
                        input_tokens: 10,
                        output_tokens: 5,
                        reasoning_tokens: 2,
                        cache_read_tokens: 3,
                        cache_write_tokens: 1,
                        cost_usd: Some(0.01),
                    }
                },
                AgentStateEvent::Idle {
                    outcome: IdleOutcome::Succeeded
                },
            ]
        );
    }

    type Recorded = Arc<std::sync::Mutex<Option<Value>>>;

    async fn record_reply(
        State(recorded): State<Recorded>,
        Path((_sid, _rid)): Path<(String, String)>,
        Json(body): Json<Value>,
    ) -> axum::http::StatusCode {
        *recorded.lock().unwrap() = Some(body);
        axum::http::StatusCode::NO_CONTENT
    }

    #[tokio::test]
    async fn responder_posts_the_permission_decision() {
        let recorded: Recorded = Arc::new(std::sync::Mutex::new(None));
        let app = Router::new()
            .route(
                "/api/session/{sid}/permission/{rid}/reply",
                post(record_reply),
            )
            .route("/api/event", get(sse_handler))
            .with_state(recorded.clone());
        let ep = spawn_mock(app).await;
        let mut start = OpenCodeServer::new(ep)
            .start(test_context(Some("ses_test")))
            .unwrap();
        start
            .responder
            .reply("per_1", InputReply::Always)
            .await
            .unwrap();
        assert_eq!(
            recorded.lock().unwrap().clone(),
            Some(serde_json::json!({ "decision": "always" }))
        );
        // A free-form reply has no opencode equivalent.
        assert!(start
            .responder
            .reply("per_1", InputReply::Value { value: "x".into() })
            .await
            .is_err());
        if let Some(stop) = start.stop.take() {
            stop();
        }
    }

    #[test]
    fn observer_requires_a_session_id() {
        let ep = Endpoint {
            url: "http://127.0.0.1:1".to_string(),
            password: "x".to_string(),
        };
        assert!(OpenCodeServer::new(ep).start(test_context(None)).is_err());
    }
}
