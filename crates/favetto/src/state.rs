//! Shared daemon state passed to every connection handler and background task.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use chrono::Utc;
use favetto_core::auth::Token;
use favetto_core::model::{Event, EventKind};
use sqlx::SqlitePool;
use tokio_cron_scheduler::JobScheduler;

use crate::chat::ChatManager;
use crate::config::FavettoConfig;
use crate::db;
use crate::event_bus::{EventBus, ServerPush};
use crate::hooks::Hook;
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
    pub chat: ChatManager,
    /// Live config, mutable at runtime (e.g. adding a provider from the TUI).
    pub config: Arc<RwLock<FavettoConfig>>,
    /// Path the config was loaded from, for persisting runtime changes.
    pub config_path: PathBuf,
    /// Directory of task-definition `.md` files.
    pub tasks_dir: PathBuf,
    /// The live task catalog.
    pub catalog: Arc<RwLock<Vec<TaskDef>>>,
    pub scheduler: JobScheduler,
    pub pair: PairStore,
    /// Live hook list (mutable so the TUI can add notification hooks).
    pub hook_store: Arc<RwLock<Vec<Hook>>>,
}

impl State {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: SqlitePool,
        bus: EventBus,
        token: Token,
        webhooks: WebhookSecrets,
        chat: ChatManager,
        config: Arc<RwLock<FavettoConfig>>,
        config_path: PathBuf,
        tasks_dir: PathBuf,
        catalog: Arc<RwLock<Vec<TaskDef>>>,
        scheduler: JobScheduler,
        hook_store: Arc<RwLock<Vec<Hook>>>,
    ) -> Self {
        Self {
            db,
            bus,
            token,
            webhooks,
            chat,
            config,
            config_path,
            tasks_dir,
            catalog,
            scheduler,
            pair: PairStore::new(),
            hook_store,
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
                crate::metrics::inc_events();
                self.bus.publish(ServerPush::Event(event));
            }
            Err(e) => tracing::warn!(error = %e, "failed to persist event"),
        }
    }
}
