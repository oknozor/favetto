//! JSON-RPC 2.0-style wire protocol, MessagePack-encoded.
//!
//! The same [`Frame`] type is carried over the Unix socket and the WebSocket
//! transport, which is what makes "local attach" just a special case of remote
//! attach. Clients send [`Request`]s and receive [`Response`]s; servers push
//! [`Notification`]s (events, task updates, log lines) unsolicited.

use serde::{Deserialize, Serialize};

/// Correlation id for a request/response pair. The TUI client increments a counter.
pub type RequestId = u64;

/// Standard JSON-RPC error codes, plus favetto-specific ones.
pub mod error_code {
    pub const PARSE: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL: i32 = -32603;
    /// Caller not authenticated / token rejected.
    pub const UNAUTHORIZED: i32 = -32001;
}

/// Well-known client → server method names.
pub mod method {
    pub const PING: &str = "system.ping";
    pub const TASKS_LIST: &str = "tasks.list";
    pub const TASKS_CANCEL: &str = "tasks.cancel";
    pub const EVENTS_TAIL: &str = "events.tail";
    /// (Re)subscribe to the live event stream. Accepts `last_event_id` to replay
    /// missed events before switching to live delivery.
    pub const EVENTS_SUBSCRIBE: &str = "events.subscribe";
    /// Open (or reopen) a chat session for a task.
    pub const CHAT_OPEN: &str = "chat.open";
    /// Send a user message to a chat session and run one agent turn.
    pub const CHAT_SEND: &str = "chat.send";
    /// Fetch a chat session's full conversation.
    pub const CHAT_MESSAGES: &str = "chat.messages";
    /// List cron schedules.
    pub const SCHEDULES_LIST: &str = "schedules.list";
    /// Create or update a cron schedule.
    pub const SCHEDULES_UPSERT: &str = "schedules.upsert";
    /// Delete a cron schedule.
    pub const SCHEDULES_DELETE: &str = "schedules.delete";
    /// List recent notifications.
    pub const NOTIFICATIONS_LIST: &str = "notifications.list";
    /// Send a test notification through a channel.
    pub const NOTIFICATIONS_TEST: &str = "notifications.test";
}

/// Well-known server → client push (notification) method names.
pub mod push {
    pub const EVENT: &str = "event";
    pub const TASK_UPDATED: &str = "task.updated";
    pub const LOG_LINE: &str = "log.line";
}

/// A client → server request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: RequestId,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

/// A structured error carried by a [`Response`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// A server → client reply to a [`Request`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: RequestId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn ok(id: RequestId, result: serde_json::Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: RequestId, code: i32, message: impl Into<String>) -> Self {
        Self {
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

/// A unidirectional message. Clients send these rarely; servers use them for pushes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub method: String,
    pub params: serde_json::Value,
}

/// Top-level tagged envelope: exactly what a wire frame carries.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    Request(Request),
    Response(Response),
    Notification(Notification),
}
