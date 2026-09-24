//! Shared daemon state passed to every connection handler and background task.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use favetto_core::auth::Token;
use favetto_core::model::{Event, EventKind};
use parking_lot::RwLock;
use sqlx::SqlitePool;
use tokio_cron_scheduler::JobScheduler;

use crate::agents::{AgentManager, AgentRegistry};
use crate::config::FavettoConfig;
use crate::db;
use crate::event_bus::{EventBus, ServerPush};
use crate::hooks::Hook;
use crate::metrics::Metrics;
use crate::pair::PairStore;
use crate::tasks::TaskDef;
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
    pub agents: AgentManager,
    /// Configured external agents, resolved once at startup.
    pub registry: AgentRegistry,
    /// Config resolved once at startup (external agents, executor/daemon defaults).
    pub config: Arc<FavettoConfig>,
    /// Directory for SQLite + token + worktrees.
    pub data_dir: PathBuf,
    /// Directory of task-definition `.md` files.
    pub tasks_dir: PathBuf,
    /// The live task catalog.
    pub catalog: Arc<RwLock<Vec<TaskDef>>>,
    pub scheduler: JobScheduler,
    pub pair: PairStore,
    /// Live hook list (mutable so the TUI can add notification hooks).
    pub hook_store: Arc<RwLock<Vec<Hook>>>,
    /// Cached provider/model catalogs, keyed by agent name (fetched lazily).
    pub providers_cache: tokio::sync::Mutex<HashMap<String, Vec<favetto_providers::Provider>>>,
    /// Per-daemon counters rendered on `/metrics`.
    pub metrics: Metrics,
}

/// Arguments for [`State::new`], grouped into a struct so the constructor's
/// signature stays stable as the daemon gains owned resources.
pub struct StateInit {
    pub db: SqlitePool,
    pub bus: EventBus,
    pub token: Token,
    pub webhooks: WebhookSecrets,
    pub agents: AgentManager,
    pub registry: AgentRegistry,
    pub config: Arc<FavettoConfig>,
    pub data_dir: PathBuf,
    pub tasks_dir: PathBuf,
    pub catalog: Arc<RwLock<Vec<TaskDef>>>,
    pub scheduler: JobScheduler,
    pub hook_store: Arc<RwLock<Vec<Hook>>>,
}

impl State {
    pub fn new(init: StateInit) -> Self {
        Self {
            db: init.db,
            bus: init.bus,
            token: init.token,
            webhooks: init.webhooks,
            agents: init.agents,
            registry: init.registry,
            config: init.config,
            data_dir: init.data_dir,
            tasks_dir: init.tasks_dir,
            catalog: init.catalog,
            scheduler: init.scheduler,
            pair: PairStore::new(),
            hook_store: init.hook_store,
            providers_cache: tokio::sync::Mutex::new(HashMap::new()),
            metrics: Metrics::default(),
        }
    }

    /// Persist an event and broadcast it, wiring the assigned monotonic id back in.
    ///
    /// This is the single durable path for emitting events: webhook receivers and
    /// hooks all funnel through here, guaranteeing the bus only carries
    /// already-persisted records.
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
                self.metrics.inc_events();
                self.bus.publish(ServerPush::Event(event));
            }
            Err(e) => tracing::warn!(error = %e, "failed to persist event"),
        }
    }
}
