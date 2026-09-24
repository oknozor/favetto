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
    /// A task changed. Carries [`Task::summary`] — never the output blob; fetch
    /// output on demand with `tasks.get`. Boxed so the (much smaller) `Event`
    /// and `CatalogUpdated` pushes do not inherit `Task`'s size.
    TaskUpdated(Box<Task>),
    /// The task catalog changed on disk; clients should re-fetch it.
    CatalogUpdated,
}

impl ServerPush {
    /// Convert into the wire [`Notification`] the TUI client understands.
    ///
    /// Returns the serialization error instead of emitting an empty payload so
    /// callers can log and skip a push they cannot encode.
    pub fn into_notification(self) -> Result<Notification, serde_json::Error> {
        match self {
            ServerPush::Event(ev) => ev.into_notification(),
            ServerPush::TaskUpdated(t) => (*t).into_notification(),
            ServerPush::CatalogUpdated => Ok(Notification {
                method: push::CATALOG_UPDATED.to_string(),
                params: serde_json::json!({}),
            }),
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
        let n = ServerPush::CatalogUpdated
            .into_notification()
            .expect("catalog push serialization");
        assert_eq!(n.method, push::CATALOG_UPDATED);
    }

    #[test]
    fn task_updated_push_carries_the_task() {
        let task = favetto_core::model::Task {
            id: uuid::Uuid::new_v4(),
            name: "t".to_string(),
            status: favetto_core::model::TaskStatus::Pending,
            attempt: 0,
            input: serde_json::json!({}),
            output: None,
            dedupe_key: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            finished_at: None,
            error: None,
            failure: None,
            session_id: None,
            session_title: None,
            parent_id: None,
            root_id: None,
            interactive: false,
        };
        let n = ServerPush::TaskUpdated(Box::new(task))
            .into_notification()
            .expect("task push serialization");
        assert_eq!(n.method, push::TASK_UPDATED);
        assert_eq!(n.params["name"], "t");
    }
}
