//! A minimal hand-rolled metrics registry, exposed as Prometheus text on `/metrics`.
//!
//! Each [`State`](crate::state::State) owns its own registry, so counters are
//! scoped to a daemon instance instead of a process-global.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::extract::State as AxumState;

/// Counters owned by a single daemon [`State`](crate::state::State).
#[derive(Default)]
pub struct Metrics {
    tasks_total: AtomicU64,
    events_total: AtomicU64,
    notifications_total: AtomicU64,
    rpc_requests_total: AtomicU64,
}

impl Metrics {
    pub fn inc_tasks(&self) {
        self.tasks_total.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_events(&self) {
        self.events_total.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_notifications(&self) {
        self.notifications_total.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_rpc(&self) {
        self.rpc_requests_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Render in Prometheus text exposition format.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "favetto_tasks_total {}\n",
            self.tasks_total.load(Ordering::Relaxed)
        ));
        out.push_str(&format!(
            "favetto_events_total {}\n",
            self.events_total.load(Ordering::Relaxed)
        ));
        out.push_str(&format!(
            "favetto_notifications_total {}\n",
            self.notifications_total.load(Ordering::Relaxed)
        ));
        out.push_str(&format!(
            "favetto_rpc_requests_total {}\n",
            self.rpc_requests_total.load(Ordering::Relaxed)
        ));
        out
    }
}

/// axum handler for `GET /metrics`.
pub async fn metrics_handler(
    AxumState(state): AxumState<Arc<crate::state::State>>,
) -> impl axum::response::IntoResponse {
    (
        axum::http::StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.metrics.render(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::response::IntoResponse;

    /// A `State` backed by a scratch SQLite database, for exercising the handler.
    async fn test_state() -> Arc<crate::state::State> {
        let dir = std::env::temp_dir().join(format!("favetto-metrics-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::db::open(&dir.join("test.db")).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        Arc::new(crate::state::State::new(
            pool,
            crate::event_bus::EventBus::new(8),
            favetto_core::auth::Token::generate(),
            crate::webhooks::WebhookSecrets::from_config(&crate::config::FavettoConfig::default()),
            crate::agents::AgentManager::new(),
            crate::agents::AgentRegistry::default(),
            Arc::new(crate::config::FavettoConfig::default()),
            dir.clone(),
            dir.clone(),
            Arc::new(parking_lot::RwLock::new(Vec::new())),
            tokio_cron_scheduler::JobScheduler::new().await.unwrap(),
            Arc::new(parking_lot::RwLock::new(Vec::new())),
        ))
    }

    #[test]
    fn render_lists_every_counter_at_zero() {
        assert_eq!(
            Metrics::default().render(),
            "favetto_tasks_total 0\n\
             favetto_events_total 0\n\
             favetto_notifications_total 0\n\
             favetto_rpc_requests_total 0\n"
        );
    }

    #[test]
    fn registries_are_independent() {
        let a = Metrics::default();
        let b = Metrics::default();
        a.inc_tasks();
        a.inc_tasks();
        a.inc_rpc();

        assert!(a.render().contains("favetto_tasks_total 2"));
        assert!(a.render().contains("favetto_rpc_requests_total 1"));
        // A second registry is untouched: there is no process-global counter.
        assert_eq!(
            b.render(),
            "favetto_tasks_total 0\n\
             favetto_events_total 0\n\
             favetto_notifications_total 0\n\
             favetto_rpc_requests_total 0\n"
        );
    }

    #[tokio::test]
    async fn handler_renders_the_states_registry() {
        let state = test_state().await;
        state.metrics.inc_events();
        state.metrics.inc_events();

        let resp = metrics_handler(AxumState(state)).await.into_response();
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();

        assert!(body.contains("favetto_events_total 2"), "{body}");
        assert!(body.contains("favetto_tasks_total 0"), "{body}");
    }
}
