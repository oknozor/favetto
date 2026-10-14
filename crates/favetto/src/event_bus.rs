//! In-process pub/sub hub plus the push types carried on it.
//!
//! The hub is intentionally simple: a `tokio::sync::broadcast` channel. Event and
//! task emitters persist to SQLite *before* publishing, so those pushes are
//! durable. `agent.event` is the one non-durable push (it carries no event id and
//! is backfilled via `tasks.events`). Slow clients that fall behind get a `Lagged`
//! error; durable pushes can be caught up via `events.subscribe { last_event_id }`
//! because the event log is append-only.

use sqlx::SqlitePool;
use tokio::sync::{broadcast, mpsc};

use favetto_core::model::{Event, Task};
use favetto_core::rpc::{push, AgentEventPush, Notification};

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
    /// One normalized agent-state event (non-durable). Carried on the bus so it
    /// reaches every client, including the stateless HTTP/SSE web client; the
    /// bounded per-session ring buffer behind `tasks.events` serves backfill.
    AgentEvent(AgentEventPush),
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
            ServerPush::AgentEvent(ev) => Ok(Notification {
                method: push::AGENT_EVENT.to_string(),
                params: serde_json::to_value(ev)?,
            }),
        }
    }

    /// The resume-cursor id this push advances, if any. Only durable `Event`
    /// pushes carry one, so the cursor never moves on `task.updated` /
    /// `catalog.updated`.
    pub fn event_id(&self) -> Option<i64> {
        match self {
            ServerPush::Event(ev) => Some(ev.id),
            _ => None,
        }
    }
}

/// What to do when a live subscriber falls behind the broadcast buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnLag {
    /// End the stream. SSE uses this so the client reconnects and replays
    /// from its persisted cursor.
    End,
    /// Keep following; skipped frames are self-contained and the client
    /// recovers them on its next full read / reconnect. Used by the WS
    /// connection pusher, which must not silently stop.
    Continue,
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

    /// Replay persisted events strictly after `last_event_id`, then stream live
    /// pushes. Both the SSE handler and the WS `events.subscribe` handler use
    /// this so resume semantics cannot diverge.
    ///
    /// The bus subscription is created synchronously here, *before* the replay
    /// query runs in the spawned task, so no event emitted during the query is
    /// missed. A push delivered both by replay and live is at-least-once; clients
    /// dedupe it against their monotonic cursor.
    ///
    /// `on_lag` selects whether falling behind ends the stream (`End`, SSE) or is
    /// logged and skipped (`Continue`, WS).
    pub fn resumable(
        &self,
        db: SqlitePool,
        last_event_id: Option<i64>,
        replay_limit: i64,
        on_lag: OnLag,
    ) -> mpsc::Receiver<ServerPush> {
        let mut live = self.subscribe();
        let (tx, rx) = mpsc::channel(256);
        tokio::spawn(async move {
            if let Some(after) = last_event_id {
                match crate::db::events_after(&db, after, replay_limit).await {
                    Ok(events) => {
                        for ev in events {
                            if tx.send(ServerPush::Event(ev)).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "event replay failed"),
                }
            }
            loop {
                match live.recv().await {
                    Ok(push) => {
                        if tx.send(push).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => match on_lag {
                        OnLag::End => {
                            tracing::warn!(n, "subscriber lagged; ending stream for cursor resume");
                            return;
                        }
                        OnLag::Continue => {
                            tracing::warn!(
                                n,
                                "client fell behind; events skipped (resume via last_event_id)"
                            );
                        }
                    },
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        rx
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
    fn agent_event_push_is_non_durable() {
        use favetto_core::model::AgentStateEvent;
        use favetto_core::rpc::AgentEventPush;

        let push = ServerPush::AgentEvent(AgentEventPush {
            session_id: "sess-1".to_string(),
            task_id: Some("task-1".to_string()),
            seq: 4,
            event: AgentStateEvent::TurnStarted,
        });
        assert_eq!(push.event_id(), None);
        let n = push.into_notification().expect("agent event serialization");
        assert_eq!(n.method, push_method());
        assert_eq!(n.params["session_id"], "sess-1");
        assert_eq!(n.params["seq"], 4);
        assert_eq!(n.params["event"]["event"], "turn_started");
    }

    fn push_method() -> &'static str {
        favetto_core::rpc::push::AGENT_EVENT
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

    #[test]
    fn event_id_only_advances_for_durable_events() {
        let event = Event {
            id: 7,
            kind: favetto_core::model::EventKind::TaskIdle,
            payload: serde_json::json!({}),
            created_at: chrono::Utc::now(),
        };
        assert_eq!(ServerPush::Event(event).event_id(), Some(7));
        assert_eq!(ServerPush::CatalogUpdated.event_id(), None);
        assert_eq!(
            ServerPush::TaskUpdated(Box::new(favetto_core::model::Task {
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
            }))
            .event_id(),
            None
        );
    }

    /// A scratch SQLite pool for the resumable tests.
    async fn test_pool() -> (SqlitePool, std::path::PathBuf) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("favetto-event-bus-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::db::open(&dir.join("test.db")).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        (pool, dir)
    }

    async fn insert_event(pool: &SqlitePool, n: i64) {
        let event = Event {
            id: 0,
            kind: favetto_core::model::EventKind::TaskIdle,
            payload: serde_json::json!({ "n": n }),
            created_at: chrono::Utc::now(),
        };
        crate::db::insert_event(pool, &event).await.unwrap();
    }

    #[tokio::test]
    async fn resumable_replays_then_follows_live() {
        let (pool, dir) = test_pool().await;
        insert_event(&pool, 1).await;
        insert_event(&pool, 2).await;

        let bus = EventBus::new(8);
        let mut rx = bus.resumable(pool.clone(), Some(0), 500, OnLag::End);

        let first = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("replay timed out")
            .expect("a replayed event");
        assert!(matches!(first, ServerPush::Event(ev) if ev.id == 1));
        let second = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("replay timed out")
            .expect("a replayed event");
        assert!(matches!(second, ServerPush::Event(ev) if ev.id == 2));

        // Nothing is replayed from the cursor onwards; live pushes still arrive.
        bus.publish(ServerPush::CatalogUpdated);
        let live = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("live push timed out")
            .expect("a live push");
        assert!(matches!(live, ServerPush::CatalogUpdated));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn resumable_lagged_ends_the_stream() {
        let (pool, dir) = test_pool().await;
        let bus = EventBus::new(1);
        let mut rx = bus.resumable(pool, None, 500, OnLag::End);

        // The single-threaded test runtime cannot poll the forwarding task
        // between these synchronous sends, so the 1-slot broadcast buffer
        // overflows and the next receive is `Lagged`.
        bus.publish(ServerPush::CatalogUpdated);
        bus.publish(ServerPush::CatalogUpdated);
        bus.publish(ServerPush::CatalogUpdated);

        let ended = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("a lagged stream must end promptly");
        assert!(ended.is_none(), "OnLag::End must close the stream");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn resumable_lagged_continues_when_asked() {
        let (pool, dir) = test_pool().await;
        let bus = EventBus::new(1);
        let mut rx = bus.resumable(pool, None, 500, OnLag::Continue);

        bus.publish(ServerPush::CatalogUpdated);
        bus.publish(ServerPush::CatalogUpdated);
        bus.publish(ServerPush::CatalogUpdated);

        let kept = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("the surviving push timed out")
            .expect("a surviving push");
        assert!(matches!(kept, ServerPush::CatalogUpdated));
        // The stream is still open: only the skipped frames were dropped.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
                .await
                .is_err(),
            "OnLag::Continue must keep the stream open"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
