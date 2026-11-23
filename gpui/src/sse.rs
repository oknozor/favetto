//! Server-Sent Events reader for the daemon's authenticated `/events` stream.
//!
//! A browser `EventSource` cannot set an `Authorization` header, and it cannot
//! resume from an explicit cursor, so the desktop client issues a streaming
//! `reqwest` request instead. This module owns the parts that must be right:
//!
//! - an incremental wire parser ([`SseParser`]) that handles CRLF, comments,
//!   multi-line `data:`, and UTF-8 sequences split across network chunks;
//! - a monotonic resume cursor ([`advance_cursor`]) so a duplicated or
//!   out-of-order frame can never move `Last-Event-ID` backwards;
//! - exponential reconnect backoff ([`Backoff`]);
//! - a 401 stop, because a rotated token will never fix itself by retrying.
//!
//! The parser, cursor and backoff are pure and unit-tested on the host; only
//! [`run`] touches the network.

use favetto_wire::model::{AgentStateEvent, Event, Task};
use favetto_wire::rpc::{push, AgentEventPush, AgentExitPush, AgentStatePush};
use serde::Deserialize;

/// The outer shape of an `agent.event` push. The event itself stays raw until
/// it is known to be renderable, so an event kind this client does not know can
/// be skipped instead of failing the whole frame.
#[derive(Deserialize)]
struct RawAgentEventPush {
    session_id: String,
    #[serde(default)]
    task_id: Option<String>,
    seq: u64,
    event: serde_json::Value,
}

/// One dispatched SSE frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseEvent {
    /// The `id:` field. Carries the durable event id for `event` frames and is
    /// absent for `task.updated` / `catalog.updated` pushes.
    pub id: Option<String>,
    /// The `event:` field (the notification method).
    pub event: Option<String>,
    /// The `data:` payload; multiple `data:` lines are joined with `\n`.
    pub data: String,
    /// The `retry:` reconnection hint, in milliseconds.
    pub retry: Option<u64>,
}

impl SseEvent {
    /// The durable event id, parsed. `None` for non-cursor pushes.
    pub fn cursor(&self) -> Option<i64> {
        self.id.as_deref().and_then(|id| id.trim().parse().ok())
    }
}

/// A decoded server push.
///
/// `SseEvent` is the wire frame; this is the typed value the UI sees. Unknown
/// methods are preserved so a newer daemon does not break an older client.
#[derive(Debug, Clone)]
pub enum Push {
    /// A durable event (`event`).
    Event(Event),
    /// A task row changed (`task.updated`).
    TaskUpdated(Box<Task>),
    /// The catalog changed on disk; re-fetch it (`catalog.updated`).
    CatalogUpdated,
    /// One normalized agent-state event (`agent.event`).
    AgentEvent(AgentEventPush),
    /// An `agent.event` whose kind this client does not know yet. The frame is
    /// deliberately ignored rather than failed, so a newer daemon cannot break
    /// the feed (docs step 06).
    UnsupportedAgentEvent {
        /// The session the skipped event belonged to.
        session_id: String,
        /// The skipped event's sequence number, for diagnostics.
        seq: u64,
    },
    /// A session's folded live state changed (`agent.state`).
    AgentState(AgentStatePush),
    /// An agent session's child process exited (`agent.exit`).
    AgentExit(AgentExitPush),
    /// A method this client does not understand yet.
    Unknown(String),
}

impl Push {
    /// A short status-line summary, e.g. `task plan (Running)`.
    pub fn label(&self) -> String {
        match self {
            Self::Event(event) => format!("event #{}", event.id),
            Self::TaskUpdated(task) => format!("task {} ({:?})", task.name, task.status),
            Self::CatalogUpdated => "catalog updated".to_string(),
            Self::AgentEvent(event) => format!("agent event {} #{}", event.session_id, event.seq),
            Self::UnsupportedAgentEvent { session_id, seq } => {
                format!("agent event {session_id} #{seq} (unsupported)")
            }
            Self::AgentState(event) => format!("agent state {}", event.session_id),
            Self::AgentExit(event) => format!("agent exit {}", event.session_id),
            Self::Unknown(method) => method.clone(),
        }
    }
}

/// A push whose JSON payload did not match its declared method.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("malformed {method} push: {message}")]
pub struct PushError {
    /// The notification method.
    pub method: String,
    /// The serde message.
    pub message: String,
}

/// Decode one dispatched frame into a typed [`Push`].
///
/// Frames without an `event:` field are ignored (`Ok(None)`), matching the
/// daemon, which always names its pushes. Unknown methods become
/// [`Push::Unknown`] rather than an error so forward compatibility is free.
/// An `agent.event` with an unknown event kind becomes
/// [`Push::UnsupportedAgentEvent`] for the same reason.
pub fn decode_push(event: &SseEvent) -> Result<Option<Push>, PushError> {
    let Some(method) = event.event.as_deref() else {
        return Ok(None);
    };
    let params: serde_json::Value = serde_json::from_str(&event.data).map_err(|err| PushError {
        method: method.to_string(),
        message: err.to_string(),
    })?;

    if method == push::CATALOG_UPDATED {
        return Ok(Some(Push::CatalogUpdated));
    }

    let failure = |message: String| PushError {
        method: method.to_string(),
        message,
    };
    let decoded = match method {
        push::EVENT => serde_json::from_value::<Event>(params).map(Push::Event),
        push::TASK_UPDATED => {
            serde_json::from_value::<Task>(params).map(|task| Push::TaskUpdated(Box::new(task)))
        }
        push::AGENT_EVENT => return decode_agent_event(params).map(Some).map_err(failure),
        push::AGENT_STATE => serde_json::from_value::<AgentStatePush>(params).map(Push::AgentState),
        push::AGENT_EXIT => serde_json::from_value::<AgentExitPush>(params).map(Push::AgentExit),
        _ => return Ok(Some(Push::Unknown(method.to_string()))),
    };
    decoded.map(Some).map_err(|err| failure(err.to_string()))
}

/// Decode an `agent.event` push, tolerating an event kind this client does not
/// know. The envelope must still be well-formed; only the inner event may be
/// unsupported.
fn decode_agent_event(params: serde_json::Value) -> Result<Push, String> {
    let raw: RawAgentEventPush = serde_json::from_value(params).map_err(|err| err.to_string())?;
    match serde_json::from_value::<AgentStateEvent>(raw.event) {
        Ok(event) => Ok(Push::AgentEvent(AgentEventPush {
            session_id: raw.session_id,
            task_id: raw.task_id,
            seq: raw.seq,
            event,
        })),
        Err(_) => Ok(Push::UnsupportedAgentEvent {
            session_id: raw.session_id,
            seq: raw.seq,
        }),
    }
}

/// Incremental SSE frame parser.
///
/// Feed it the byte chunks of a `text/event-stream` body; it emits a [`SseEvent`]
/// for each blank-line-terminated frame. Bytes are buffered until a full line is
/// available, so a multi-byte UTF-8 sequence split across two network chunks is
/// decoded intact.
#[derive(Debug, Default)]
pub struct SseParser {
    buffer: Vec<u8>,
    id: Option<String>,
    event: Option<String>,
    data: Vec<String>,
    retry: Option<u64>,
}

impl SseParser {
    /// A fresh parser with an empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of text, returning every frame it completes.
    #[allow(dead_code)] // Convenience for tests and future string-based callers.
    pub fn push(&mut self, chunk: &str) -> Vec<SseEvent> {
        self.push_bytes(chunk.as_bytes())
    }

    /// Feed a chunk of raw bytes, returning every frame it completes.
    pub fn push_bytes(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(newline) = self.buffer.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=newline).collect();
            line.pop(); // trailing '\n'
            let line = line.strip_suffix(b"\r").unwrap_or(&line).to_vec();
            let line = String::from_utf8_lossy(&line).into_owned();
            self.line(&line, &mut events);
        }
        events
    }

    fn line(&mut self, line: &str, out: &mut Vec<SseEvent>) {
        if line.is_empty() {
            // The spec only dispatches when the data buffer is non-empty, which
            // is exactly the daemon's shape (every frame carries a JSON payload).
            if !self.data.is_empty() {
                out.push(self.take_event());
            }
            return;
        }
        // A line starting with ':' is a comment (the `: keep-alive` heartbeat).
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "id" => {
                if !value.contains('\0') {
                    self.id = Some(value.to_string());
                }
            }
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            "retry" => {
                if let Ok(ms) = value.trim().parse::<u64>() {
                    self.retry = Some(ms);
                }
            }
            // Unknown fields are ignored per the spec.
            _ => {}
        }
    }

    fn take_event(&mut self) -> SseEvent {
        SseEvent {
            id: self.id.take(),
            event: self.event.take(),
            data: std::mem::take(&mut self.data).join("\n"),
            retry: self.retry.take(),
        }
    }
}

/// Move a cursor forward to the event's id, never backwards.
pub fn advance_cursor(current: Option<i64>, event: &SseEvent) -> Option<i64> {
    match event.cursor() {
        Some(id) => Some(current.map_or(id, |c| c.max(id))),
        None => current,
    }
}

/// Exponential reconnect backoff, capped so a long outage stays polite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    base_ms: u64,
    max_ms: u64,
}

impl Backoff {
    /// A `base_ms`-first schedule that never exceeds `max_ms`.
    pub const fn new(base_ms: u64, max_ms: u64) -> Self {
        Self { base_ms, max_ms }
    }

    /// The delay before the connection attempt numbered `attempt` (0-based):
    /// `base`, `2*base`, `4*base`, … capped at `max_ms`.
    pub fn delay_ms(&self, attempt: u32) -> u64 {
        let shift = attempt.min(20);
        self.base_ms.saturating_mul(1u64 << shift).min(self.max_ms)
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(1_000, 30_000)
    }
}

/// A connection-state transition emitted by [`run`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseStatus {
    /// The stream is open and receiving frames.
    Connected,
    /// The stream dropped; the reader will retry after `delay_ms`.
    Reconnecting { attempt: u32, delay_ms: u64 },
    /// The daemon answered 401: the token is missing or has been rotated. The
    /// reader stops retrying so the UI can point the user at the token file.
    Unauthorized,
}

/// Errors from a single streaming attempt.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SseError {
    /// The request never reached the daemon.
    #[error("network: {0}")]
    Network(String),
    /// The daemon answered with a non-2xx status.
    #[error("http error {0}")]
    Http(u16),
}

impl SseError {
    /// True for errors that retrying cannot fix.
    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::Http(401))
    }
}

/// Open one streaming request and return the live response.
async fn start_stream(
    http: &reqwest::Client,
    url: &str,
    token: Option<&str>,
    last_event_id: Option<i64>,
) -> Result<reqwest::Response, SseError> {
    let url = match last_event_id {
        Some(id) if url.contains('?') => format!("{url}&last_event_id={id}"),
        Some(id) => format!("{url}?last_event_id={id}"),
        None => url.to_string(),
    };

    let mut request = http
        .get(&url)
        .header(reqwest::header::ACCEPT, "text/event-stream");
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    if let Some(id) = last_event_id {
        request = request.header("Last-Event-ID", id.to_string());
    }

    let response = request
        .send()
        .await
        .map_err(|err| SseError::Network(err.to_string()))?;
    let status = response.status().as_u16();
    if !(200..=299).contains(&status) {
        return Err(SseError::Http(status));
    }
    Ok(response)
}

/// Stream `{endpoint}/events`, reconnecting with backoff.
///
/// `on_event` receives every dispatched frame together with the advanced resume
/// cursor; `on_status` receives connection transitions so the UI can render a
/// banner. Both are invoked from the spawned task, so they must be `Send`.
///
/// The loop exits only when the task is aborted (the runtime aborts it on
/// shutdown/reconnect) or when the daemon answers 401 — retrying a rejected
/// token forever would spin and conceal the fix.
pub async fn run<E, S>(
    url: String,
    token: Option<String>,
    initial_cursor: Option<i64>,
    mut on_event: E,
    mut on_status: S,
) where
    E: FnMut(SseEvent, Option<i64>) + Send + 'static,
    S: FnMut(SseStatus) + Send + 'static,
{
    use futures_util::StreamExt;

    let backoff = Backoff::default();
    let http = match reqwest::Client::builder().build() {
        Ok(client) => client,
        Err(_err) => {
            // Building a plain HTTP client should not fail; if it does, report a
            // retryable state instead of panicking inside the runtime.
            on_status(SseStatus::Reconnecting {
                attempt: 1,
                delay_ms: backoff.delay_ms(0),
            });
            return;
        }
    };
    let mut attempt: u32 = 0;
    let mut cursor = initial_cursor;

    loop {
        match start_stream(&http, &url, token.as_deref(), cursor).await {
            Ok(response) => {
                on_status(SseStatus::Connected);
                attempt = 0;
                let mut stream = response.bytes_stream();
                let mut parser = SseParser::new();
                while let Some(chunk) = stream.next().await {
                    let Ok(bytes) = chunk else { break };
                    for event in parser.push_bytes(&bytes) {
                        cursor = advance_cursor(cursor, &event);
                        on_event(event, cursor);
                    }
                }
            }
            Err(err) if err.is_fatal() => {
                // A rotated or revoked token: stop and let the UI point at it.
                on_status(SseStatus::Unauthorized);
                return;
            }
            Err(_) => {
                // Network/HTTP errors are transient; back off and retry.
            }
        }

        let delay = backoff.delay_ms(attempt);
        attempt = attempt.saturating_add(1);
        on_status(SseStatus::Reconnecting {
            attempt,
            delay_ms: delay,
        });
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(chunks: &[&str]) -> Vec<SseEvent> {
        let mut parser = SseParser::new();
        let mut events = Vec::new();
        for chunk in chunks {
            events.extend(parser.push(chunk));
        }
        events
    }

    #[test]
    fn parses_id_event_and_data() {
        let events = parse_all(&["id: 42\nevent: event\ndata: {\"id\":42}\n\n"]);
        assert_eq!(
            events,
            vec![SseEvent {
                id: Some("42".into()),
                event: Some("event".into()),
                data: "{\"id\":42}".into(),
                retry: None,
            }]
        );
    }

    #[test]
    fn joins_multiple_data_lines() {
        let events = parse_all(&["data: one\ndata: two\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "one\ntwo");
    }

    #[test]
    fn ignores_comments_and_handles_a_split_frame() {
        let mut parser = SseParser::new();
        // A heartbeat comment produces nothing and must not disturb the buffer.
        assert!(parser.push(": keep-alive\n").is_empty());
        // The frame arrives across two chunks, mid-line.
        assert!(parser.push("id: 7\nevent: ta").is_empty());
        let events = parser.push("sk.updated\ndata: {}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("task.updated"));
        assert_eq!(events[0].id.as_deref(), Some("7"));
        assert_eq!(events[0].data, "{}");
    }

    #[test]
    fn handles_crlf_and_a_missing_space_after_the_colon() {
        let events = parse_all(&["id:9\r\ndata:x\r\n\r\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id.as_deref(), Some("9"));
        assert_eq!(events[0].data, "x");
    }

    #[test]
    fn parses_retry_and_ignores_unknown_fields() {
        let events = parse_all(&["retry: 2500\nfoo: bar\ndata: hi\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].retry, Some(2500));
        assert_eq!(events[0].data, "hi");
    }

    #[test]
    fn a_frame_without_data_is_not_dispatched() {
        let events = parse_all(&["id: 1\nevent: ping\n\n"]);
        assert!(events.is_empty());
    }

    /// A multi-byte UTF-8 character split across two network chunks is decoded
    /// intact: the parser buffers bytes, not lossy strings.
    #[test]
    fn utf8_split_across_chunks_is_intact() {
        let text = "data: héllo ✓\n\n";
        let bytes = text.as_bytes();
        let split = text.find('é').expect("é") + 1; // mid-character
        let mut parser = SseParser::new();
        assert!(parser.push_bytes(&bytes[..split]).is_empty());
        let events = parser.push_bytes(&bytes[split..]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "héllo ✓");
    }

    #[test]
    fn cursor_advances_monotonically() {
        let mut cursor = None;
        for id in [3, 1, 5, 5] {
            let event = SseEvent {
                id: Some(id.to_string()),
                ..SseEvent::default()
            };
            cursor = advance_cursor(cursor, &event);
        }
        assert_eq!(cursor, Some(5));
        // A non-cursor push leaves the cursor untouched.
        assert_eq!(advance_cursor(cursor, &SseEvent::default()), Some(5));
        // A malformed id is ignored rather than resetting the cursor.
        let malformed = SseEvent {
            id: Some("not-a-number".into()),
            ..SseEvent::default()
        };
        assert_eq!(advance_cursor(cursor, &malformed), Some(5));
    }

    #[test]
    fn backoff_doubles_then_caps() {
        let backoff = Backoff::new(1_000, 30_000);
        assert_eq!(backoff.delay_ms(0), 1_000);
        assert_eq!(backoff.delay_ms(1), 2_000);
        assert_eq!(backoff.delay_ms(2), 4_000);
        assert_eq!(backoff.delay_ms(3), 8_000);
        assert_eq!(backoff.delay_ms(5), 30_000);
        assert_eq!(backoff.delay_ms(100), 30_000);
    }

    #[test]
    fn only_401_is_fatal_for_reconnection() {
        assert!(SseError::Http(401).is_fatal());
        assert!(!SseError::Http(500).is_fatal());
        assert!(!SseError::Http(404).is_fatal());
        assert!(!SseError::Network("offline".into()).is_fatal());
    }

    #[test]
    fn decodes_durable_events_and_ignores_frames_without_an_event_field() {
        let event = SseEvent {
            id: Some("9".into()),
            event: Some(push::EVENT.to_string()),
            data: serde_json::json!({
                "id": 9,
                "kind": "task_finished",
                "payload": { "id": 9 },
                "created_at": "2026-01-01T00:00:00Z",
            })
            .to_string(),
            retry: None,
        };
        match decode_push(&event).expect("decode") {
            Some(Push::Event(decoded)) => assert_eq!(decoded.id, 9),
            other => panic!("expected an event push, got {other:?}"),
        }

        let anonymous = SseEvent {
            data: "{}".into(),
            ..SseEvent::default()
        };
        assert!(decode_push(&anonymous).expect("ignored").is_none());
    }

    #[test]
    fn catalog_updated_needs_no_payload() {
        let event = SseEvent {
            event: Some(push::CATALOG_UPDATED.to_string()),
            data: "{}".into(),
            ..SseEvent::default()
        };
        assert!(matches!(
            decode_push(&event).expect("decode"),
            Some(Push::CatalogUpdated)
        ));
    }

    #[test]
    fn unknown_methods_and_malformed_payloads_are_values() {
        let unknown = SseEvent {
            event: Some("future.push".into()),
            data: "{}".into(),
            ..SseEvent::default()
        };
        match decode_push(&unknown).expect("decode") {
            Some(Push::Unknown(method)) => assert_eq!(method, "future.push"),
            other => panic!("expected an unknown push, got {other:?}"),
        }

        let malformed = SseEvent {
            event: Some(push::TASK_UPDATED.to_string()),
            data: "not json".into(),
            ..SseEvent::default()
        };
        let error = decode_push(&malformed).expect_err("malformed");
        assert_eq!(error.method, push::TASK_UPDATED);
        assert!(!error.message.is_empty());
    }

    #[test]
    fn unknown_agent_event_kinds_are_skipped_not_fatal() {
        let frame = SseEvent {
            event: Some(push::AGENT_EVENT.to_string()),
            data: serde_json::json!({
                "session_id": "ses_1",
                "task_id": "task-1",
                "seq": 7,
                "event": { "event": "future_kind", "detail": 1 },
            })
            .to_string(),
            ..SseEvent::default()
        };
        match decode_push(&frame).expect("decode") {
            Some(Push::UnsupportedAgentEvent { session_id, seq }) => {
                assert_eq!(session_id, "ses_1");
                assert_eq!(seq, 7);
            }
            other => panic!("expected an unsupported agent event, got {other:?}"),
        }

        // A malformed envelope is still an error, not silently dropped.
        let broken = SseEvent {
            event: Some(push::AGENT_EVENT.to_string()),
            data: "{\"session_id\":\"ses_1\"}".into(),
            ..SseEvent::default()
        };
        assert!(decode_push(&broken).is_err());
    }
}
