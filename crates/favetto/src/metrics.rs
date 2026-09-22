//! A minimal hand-rolled metrics registry, exposed as Prometheus text on `/metrics`.
//!
//! Counters live in a global [`METRICS`] so any module can increment them without
//! threading a handle through the call graph.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;

pub static METRICS: LazyLock<Metrics> = LazyLock::new(Metrics::default);

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

pub fn inc_tasks() {
    METRICS.inc_tasks();
}
pub fn inc_events() {
    METRICS.inc_events();
}
pub fn inc_notifications() {
    METRICS.inc_notifications();
}
pub fn inc_rpc() {
    METRICS.inc_rpc();
}

/// axum handler for `GET /metrics`.
pub async fn metrics_handler() -> impl axum::response::IntoResponse {
    (
        axum::http::StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        METRICS.render(),
    )
}
