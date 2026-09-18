//! A minimal hand-rolled metrics registry, exposed as Prometheus text on `/metrics`.
//!
//! Counters live in a global [`METRICS`] so any module can increment them without
//! threading a handle through the call graph. Per-MCP-server tool stats are kept in
//! a small mutex-guarded map (low-frequency writes).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

pub static METRICS: LazyLock<Metrics> = LazyLock::new(Metrics::default);

#[derive(Default)]
pub struct Metrics {
    tasks_total: AtomicU64,
    events_total: AtomicU64,
    notifications_total: AtomicU64,
    rpc_requests_total: AtomicU64,
    mcp_tool_calls_total: AtomicU64,
    mcp_tool_errors_total: AtomicU64,
    tools: Mutex<BTreeMap<String, ToolStats>>,
}

#[derive(Default, Clone, Copy)]
struct ToolStats {
    calls: u64,
    errors: u64,
    latency_ms_sum: u64,
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
    pub fn record_tool_call(&self, server: &str, tool: &str, latency_ms: u64, ok: bool) {
        self.mcp_tool_calls_total.fetch_add(1, Ordering::Relaxed);
        if !ok {
            self.mcp_tool_errors_total.fetch_add(1, Ordering::Relaxed);
        }
        let key = format!("{server}/{tool}");
        let mut map = self.tools.lock().unwrap();
        let entry = map.entry(key).or_default();
        entry.calls += 1;
        if !ok {
            entry.errors += 1;
        }
        entry.latency_ms_sum += latency_ms;
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
        out.push_str(&format!(
            "favetto_mcp_tool_calls_total {}\n",
            self.mcp_tool_calls_total.load(Ordering::Relaxed)
        ));
        out.push_str(&format!(
            "favetto_mcp_tool_errors_total {}\n",
            self.mcp_tool_errors_total.load(Ordering::Relaxed)
        ));

        let map = self.tools.lock().unwrap();
        for (key, stats) in map.iter() {
            let (server, tool) = key.split_once('/').unwrap_or((key, ""));
            out.push_str(&format!(
                "favetto_mcp_tool_calls{{server=\"{server}\",tool=\"{tool}\"}} {}\n",
                stats.calls
            ));
            out.push_str(&format!(
                "favetto_mcp_tool_errors{{server=\"{server}\",tool=\"{tool}\"}} {}\n",
                stats.errors
            ));
            out.push_str(&format!(
                "favetto_mcp_tool_latency_ms_sum{{server=\"{server}\",tool=\"{tool}\"}} {}\n",
                stats.latency_ms_sum
            ));
        }
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
pub fn record_tool_call(server: &str, tool: &str, latency_ms: u64, ok: bool) {
    METRICS.record_tool_call(server, tool, latency_ms, ok);
}

/// axum handler for `GET /metrics`.
pub async fn metrics_handler() -> impl axum::response::IntoResponse {
    (
        axum::http::StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        METRICS.render(),
    )
}
