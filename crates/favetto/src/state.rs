//! Shared daemon state passed to every connection handler and background task.

use chrono::Utc;
use favetto_core::auth::Token;
use favetto_core::model::{Event, EventKind};
use sqlx::SqlitePool;

use crate::chat::ChatManager;
use crate::db;
use crate::event_bus::{EventBus, ServerPush};
use crate::webhooks::WebhookSecrets;

/// Everything the daemon owns that connections and background tasks need access to.
///
/// `db` is durable state, `bus` is the in-process pub/sub hub, and `token` guards
/// the remote (WebSocket) surface. Wrapped in `Arc` and cloned per-connection.
pub struct State {
    pub db: SqlitePool,
    pub bus: EventBus,
    pub token: Token,
    pub webhooks: WebhookSecrets,
    pub chat: ChatManager,
}

impl State {
    pub fn new(
        db: SqlitePool,
        bus: EventBus,
        token: Token,
        webhooks: WebhookSecrets,
        chat: ChatManager,
    ) -> Self {
        Self {
            db,
            bus,
            token,
            webhooks,
            chat,
        }
    }

    /// Persist an event and broadcast it, wiring the assigned monotonic id back in.
    ///
    /// This is the single durable path for emitting events: webhook receivers, the
    /// synthetic driver, and hooks all funnel through here, guaranteeing the bus only
    /// carries already-persisted records.
    pub async fn emit_event(&self, kind: EventKind, payload: serde_json::Value) {
        let event = Event {
            id: 0,
            kind,
            payload,
            created_at: Utc::now(),
        };
        match db::insert_event(&self.db, &event).await {
            Ok(id) => {
                let event = Event { id, ..event };
                self.bus.publish(ServerPush::Event(event));
            }
            Err(e) => tracing::warn!(error = %e, "failed to persist event"),
        }
    }
}
