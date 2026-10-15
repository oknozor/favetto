//! Server-Sent Events reader over a streaming `fetch`.
//!
//! The browser `EventSource` API cannot set an `Authorization` header and reuses
//! its URL on auto-reconnect, so it cannot drive the daemon's bearer-authenticated
//! `/events` stream. Instead the client issues a streaming `fetch`, sets the
//! bearer header itself, parses the SSE wire format incrementally, persists the
//! highest `id:` it has seen, and reconnects with exponential backoff, resuming
//! from that cursor.
//!
//! The parser, the cursor advance and the backoff schedule are pure and unit
//! tested on the host; only [`run`] touches the browser.

/// One dispatched SSE frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseEvent {
    /// The `id:` field, when present. Carries the durable event id for `event`
    /// frames and is absent for `task.updated` / `catalog.updated` pushes.
    pub id: Option<String>,
    /// The `event:` field (the notification method).
    pub event: Option<String>,
    /// The `data:` payload; multiple `data:` lines are joined with `\n`.
    pub data: String,
    /// The `retry:` reconnection hint, in milliseconds.
    pub retry: Option<u64>,
}

impl SseEvent {
    /// True for a frame with no fields (an SSE comment/heartbeat is consumed by
    /// the parser and never surfaces as an event).
    pub fn is_empty(&self) -> bool {
        self.id.is_none() && self.event.is_none() && self.data.is_empty()
    }

    /// The durable event id, parsed. `None` for non-cursor pushes.
    pub fn cursor(&self) -> Option<i64> {
        self.id.as_deref().and_then(|id| id.trim().parse().ok())
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
    /// reader stops retrying so the UI can send the user back to `/pair`.
    Unauthorized,
}

/// Errors from a single streaming-fetch attempt.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SseError {
    /// The request never reached the daemon.
    #[error("network: {0}")]
    Network(String),
    /// The daemon answered with a non-2xx status (401 = bad token).
    #[error("http error {0}")]
    Http(u16),
    /// A 2xx response with no readable body.
    #[error("response has no body")]
    NoBody,
}

/// Open one streaming-fetch request and return the live response.
#[cfg(target_arch = "wasm32")]
async fn start_stream(
    url: &str,
    token: Option<&str>,
    last_event_id: Option<i64>,
) -> Result<web_sys::Response, SseError> {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;

    fn net(err: wasm_bindgen::JsValue) -> SseError {
        SseError::Network(format!("{err:?}"))
    }

    let window = web_sys::window().ok_or_else(|| SseError::Network("no global window".into()))?;
    let headers = web_sys::Headers::new().map_err(net)?;
    headers.set("Accept", "text/event-stream").map_err(net)?;
    if let Some(token) = token {
        headers
            .set("Authorization", &format!("Bearer {token}"))
            .map_err(net)?;
    }
    let url = match last_event_id {
        Some(id) => {
            headers.set("Last-Event-ID", &id.to_string()).map_err(net)?;
            if url.contains('?') {
                format!("{url}&last_event_id={id}")
            } else {
                format!("{url}?last_event_id={id}")
            }
        }
        None => url.to_string(),
    };

    let init = web_sys::RequestInit::new();
    init.set_method("GET");
    init.set_headers(&headers);
    let request = web_sys::Request::new_with_str_and_init(&url, &init).map_err(net)?;
    let response = JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(net)?;
    let response: web_sys::Response = response
        .dyn_into()
        .map_err(|_| SseError::Network("fetch did not return a Response".into()))?;
    if !response.ok() {
        return Err(SseError::Http(response.status()));
    }
    Ok(response)
}

/// Stream `{base}/events` forever, reconnecting with backoff.
///
/// `on_event` receives every dispatched frame; `on_status` receives connection
/// transitions so the UI can render a banner. Both are invoked from the spawned
/// task, so they must be `'static` (Dioxus signals are `Copy` and qualify).
///
/// The loop exits only if the task is dropped; a dropped stream (tab closed,
/// daemon restart, proxy timeout, or the daemon's `Lagged` end-of-stream) is
/// retried from the last cursor.
#[cfg(target_arch = "wasm32")]
pub async fn run<E, S>(
    url: String,
    token: Option<String>,
    initial_cursor: Option<i64>,
    mut on_event: E,
    mut on_status: S,
) where
    E: FnMut(SseEvent) + 'static,
    S: FnMut(SseStatus) + 'static,
{
    use futures_util::StreamExt;

    let backoff = Backoff::default();
    let mut attempt: u32 = 0;
    let mut cursor = initial_cursor;

    loop {
        match start_stream(&url, token.as_deref(), cursor).await {
            Ok(response) => match response.body() {
                Some(body) => {
                    on_status(SseStatus::Connected);
                    attempt = 0;
                    let mut stream = wasm_streams::ReadableStream::from_raw(body).into_stream();
                    let mut parser = SseParser::new();
                    while let Some(chunk) = stream.next().await {
                        let Ok(chunk) = chunk else { break };
                        let bytes = js_sys::Uint8Array::new(&chunk).to_vec();
                        for event in parser.push_bytes(&bytes) {
                            cursor = advance_cursor(cursor, &event);
                            on_event(event);
                        }
                    }
                }
                None => {
                    // Treat a body-less 2xx like a disconnect and retry.
                }
            },
            Err(SseError::Http(401)) => {
                // A rotated or revoked token: retrying forever would spin. Stop
                // and let the UI clear the token and return to `/pair`.
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
        gloo_timers::future::TimeoutFuture::new(delay.min(u32::MAX as u64) as u32).await;
    }
}

/// Native transport (`reqwest`): open one streaming request and return the live
/// response. Mirrors the wasm `start_stream`, including the `Last-Event-ID`
/// header and `?last_event_id=` resume hint.
#[cfg(not(target_arch = "wasm32"))]
async fn start_stream(
    url: &str,
    token: Option<&str>,
    last_event_id: Option<i64>,
) -> Result<reqwest::Response, SseError> {
    let url = match last_event_id {
        Some(id) if url.contains('?') => format!("{url}&last_event_id={id}"),
        Some(id) => format!("{url}?last_event_id={id}"),
        None => url.to_string(),
    };

    let mut request = crate::http::client()
        .get(&url)
        .header("Accept", "text/event-stream");
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
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

/// Native equivalent of [`run`](run): stream `{base}/events` with `reqwest`,
/// reconnecting with backoff. The parser, cursor and backoff are shared with the
/// wasm path.
#[cfg(not(target_arch = "wasm32"))]
pub async fn run<E, S>(
    url: String,
    token: Option<String>,
    initial_cursor: Option<i64>,
    mut on_event: E,
    mut on_status: S,
) where
    E: FnMut(SseEvent) + 'static,
    S: FnMut(SseStatus) + 'static,
{
    use futures_util::StreamExt;

    let backoff = Backoff::default();
    let mut attempt: u32 = 0;
    let mut cursor = initial_cursor;

    loop {
        match start_stream(&url, token.as_deref(), cursor).await {
            Ok(response) => {
                on_status(SseStatus::Connected);
                attempt = 0;
                let mut stream = response.bytes_stream();
                let mut parser = SseParser::new();
                while let Some(chunk) = stream.next().await {
                    let Ok(bytes) = chunk else { break };
                    for event in parser.push_bytes(&bytes) {
                        cursor = advance_cursor(cursor, &event);
                        on_event(event);
                    }
                }
            }
            Err(SseError::Http(401)) => {
                // A rotated or revoked token: stop and let the UI clear it.
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
}
