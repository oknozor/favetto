//! Shared daemon state passed to every connection handler and background task.

use std::path::PathBuf;
use std::sync::Arc;

use crate::scheduler::Scheduler;
use favetto_core::auth::Token;
use favetto_core::model::{Event, EventKind};
use favetto_core::Timestamp;
use parking_lot::RwLock;
use sqlx::SqlitePool;

use crate::actions::ActionRegistry;
use crate::agents::{AgentManager, AgentRegistry};
use crate::auth::LoginLimiter;
use crate::config::FavettoConfig;
use crate::db;
use crate::event_bus::{EventBus, ServerPush};
use crate::mcp::McpRegistry;
use crate::metrics::Metrics;
use crate::pair::PairStore;
use crate::repos::KnownRepo;
use crate::tasks::TaskDef;

/// Everything the daemon owns that connections and background tasks need access to.
///
/// `db` is durable state, `bus` is the in-process pub/sub hub, and `token` guards
/// the remote (WebSocket) surface. Wrapped in `Arc` and cloned per-connection.
pub struct State {
    pub db: SqlitePool,
    pub bus: EventBus,
    pub token: Token,
    pub agents: AgentManager,
    /// Configured external agents, resolved once at startup.
    pub registry: AgentRegistry,
    /// Curated built-in actions a `kind = "action"` task can run.
    pub actions: ActionRegistry,
    /// Config resolved once at startup (external agents, executor/daemon defaults).
    pub config: Arc<FavettoConfig>,
    /// Resolved path of the config file the daemon was started with; re-read by
    /// `config.validate` (the in-memory `config` already fell back to defaults on
    /// a parse error, so it cannot be used to report the error).
    pub config_path: PathBuf,
    /// Directory for SQLite + token + worktrees.
    pub data_dir: PathBuf,
    /// Directory of task-definition `.md` files.
    pub tasks_dir: PathBuf,
    /// The live task catalog.
    pub catalog: Arc<RwLock<Vec<TaskDef>>>,
    pub scheduler: Scheduler,
    pub pair: PairStore,
    /// Per-`(username, client)` failed-login throttling for `POST /auth/login`.
    pub login_limiter: LoginLimiter,
    /// Per-daemon counters rendered on `/metrics`.
    pub metrics: Metrics,
    /// Known repositories whose `.favetto/tasks/` catalogs are loaded.
    pub repos: Arc<RwLock<Vec<KnownRepo>>>,
    /// The daemon-wide active repo name; `None` means the global catalog.
    pub active_repo: Arc<RwLock<Option<String>>>,
    /// Registered MCP server definitions plus in-memory literal secrets.
    pub mcp: Arc<RwLock<McpRegistry>>,
    /// Control signal to the catalog watcher: sent when the set of watched roots
    /// changes (a repo was added/removed). The watcher re-targets and reloads.
    pub watch_tx: tokio::sync::mpsc::UnboundedSender<()>,
    /// Receiver half of [`Self::watch_tx`], taken exactly once by
    /// [`crate::catalog_watch::spawn`].
    pub(crate) watch_rx: parking_lot::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<()>>>,
}

/// Arguments for [`State::new`], grouped into a struct so the constructor's
/// signature stays stable as the daemon gains owned resources.
pub struct StateInit {
    pub db: SqlitePool,
    pub bus: EventBus,
    pub token: Token,
    pub agents: AgentManager,
    pub registry: AgentRegistry,
    pub config: Arc<FavettoConfig>,
    pub config_path: PathBuf,
    pub data_dir: PathBuf,
    pub tasks_dir: PathBuf,
    pub catalog: Arc<RwLock<Vec<TaskDef>>>,
    pub scheduler: Scheduler,
}

impl State {
    pub fn new(init: StateInit) -> Self {
        let login_limiter = LoginLimiter::new(
            init.config.auth.login_rate_limit,
            init.config.auth.login_window_secs,
        );
        let (watch_tx, watch_rx) = tokio::sync::mpsc::unbounded_channel();
        let mcp = Arc::new(RwLock::new(McpRegistry::new(&init.data_dir)));
        // Attach the bus to the agent manager before it is stored, so every
        // normalized event is published as an `agent.event` push.
        let mut agents = init.agents;
        agents.set_bus(init.bus.clone());
        agents.set_store(init.db.clone());
        Self {
            db: init.db,
            bus: init.bus,
            token: init.token,
            agents,
            registry: init.registry,
            actions: ActionRegistry::new(),
            config: init.config,
            config_path: init.config_path,
            data_dir: init.data_dir,
            tasks_dir: init.tasks_dir,
            catalog: init.catalog,
            scheduler: init.scheduler,
            pair: PairStore::new(),
            login_limiter,
            metrics: Metrics::default(),
            repos: Arc::new(RwLock::new(Vec::new())),
            active_repo: Arc::new(RwLock::new(None)),
            mcp,
            watch_tx,
            watch_rx: parking_lot::Mutex::new(Some(watch_rx)),
        }
    }

    /// Persist an event and broadcast it, wiring the assigned monotonic id back in.
    ///
    /// This is the single durable path for emitting events, guaranteeing the bus
    /// only carries already-persisted records.
    pub async fn emit_event(&self, kind: EventKind, payload: serde_json::Value) {
        let event = Event {
            id: 0,
            kind,
            payload,
            created_at: Timestamp::now(),
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
