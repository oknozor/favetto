//! In-process pub/sub hub plus the push types carried on it.
//!
//! The hub is intentionally simple: a `tokio::sync::broadcast` channel. Emitters
//! persist to SQLite *before* publishing, so the bus only ever carries durable
//! records. Slow clients that fall behind get a `Lagged` error; they can catch up
//! via `events.subscribe { last_event_id }` because the event log is append-only.

use tokio::sync::broadcast;

use favetto_core::model::{Event, Task};
use favetto_core::rpc::{push, Notification};

/// Server → client push. Clients receive these as `Frame::Notification`.
#[derive(Debug, Clone)]
pub enum ServerPush {
    Event(Event),
    TaskUpdated(Task),
    /// The task catalog changed on disk; clients should re-fetch it.
    CatalogUpdated,
    /// Forward-looking: used once the runtime streams agent logs to clients.
    #[allow(dead_code)]
    LogLine { level: String, message: String },
}

impl ServerPush {
    /// Convert into the wire [`Notification`] the TUI client understands.
    pub fn into_notification(self) -> Notification {
        match self {
            ServerPush::Event(ev) => ev.into_notification(),
            ServerPush::TaskUpdated(t) => t.into_notification(),
            ServerPush::CatalogUpdated => Notification {
                method: push::CATALOG_UPDATED.to_string(),
                params: serde_json::json!({}),
            },
            ServerPush::LogLine { level, message } => Notification {
                method: push::LOG_LINE.to_string(),
                params: serde_json::json!({ "level": level, "message": message }),
            },
        }
    }
}

#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<ServerPush>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Subscribe to future pushes. New subscribers do not receive anything already
    /// broadcast — replay is handled by the persistence layer.
    pub fn subscribe(&self) -> broadcast::Receiver<ServerPush> {
        self.tx.subscribe()
    }

    /// Publish a push to every current subscriber. Best-effort: if there are no
    /// subscribers the value is dropped.
    pub fn publish(&self, push: ServerPush) {
        let _ = self.tx.send(push);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_updated_push_maps_to_notification() {
        let n = ServerPush::CatalogUpdated.into_notification();
        assert_eq!(n.method, push::CATALOG_UPDATED);
    }
}
