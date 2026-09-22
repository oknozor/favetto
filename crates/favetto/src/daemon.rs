//! The `daemon` subcommand: wire up persistence, the event bus, webhooks, hooks,
//! the scheduler, the executor, and transports, then run until a shutdown signal
//! arrives.

use std::path::PathBuf;
use std::sync::Arc;

use axum::routing::get;
use axum::Router;
use favetto_core::auth::Token;

use crate::cli::DaemonArgs;
use crate::config::FavettoConfig;
use crate::state::State;
use crate::webhooks::WebhookSecrets;
use crate::{db, event_bus, hooks, transport};

pub async fn run(args: DaemonArgs) -> anyhow::Result<()> {
    // Load config (CLI --config, then ~/.config/favetto/config.toml).
    let config_path = args
        .config
        .clone()
        .unwrap_or_else(crate::cli::default_config_path);
    let config = Arc::new(std::sync::RwLock::new(
        match FavettoConfig::load_from(&config_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, path = %config_path.display(), "failed to load config; using defaults");
                FavettoConfig::default()
            }
        },
    ));

    // Resolve settings: CLI flag > config > default. Read the config once here.
    let data_dir = {
        let cfg = config.read().unwrap();
        args.data_dir
            .clone()
            .or_else(|| cfg.daemon.data_dir.clone())
            .unwrap_or_else(crate::cli::default_data_dir)
    };
    let socket = {
        let cfg = config.read().unwrap();
        args.socket
            .clone()
            .or_else(|| cfg.daemon.socket.clone())
            .unwrap_or_else(|| PathBuf::from("/tmp/favetto.sock"))
    };
    let listen = {
        let cfg = config.read().unwrap();
        args.listen
            .clone()
            .or_else(|| cfg.daemon.listen.clone())
            .unwrap_or_else(|| "127.0.0.1:7878".to_string())
    };
    let tasks_dir = {
        let cfg = config.read().unwrap();
        args.tasks_dir
            .clone()
            .or_else(|| cfg.daemon.tasks_dir.clone())
            .unwrap_or_else(|| PathBuf::from("tasks"))
    };
    tokio::fs::create_dir_all(&data_dir).await?;

    let db_path = data_dir.join("favetto.db");
    let token_path = data_dir.join("token");

    let pool = db::open(&db_path).await?;
    db::migrate(&pool).await?;

    // Tasks left running by a previous instance are interrupted: their agent
    // process died with the old daemon, so mark them failed rather than letting
    // them linger (the executor only ever picks up `pending` tasks).
    match db::fail_interrupted_tasks(&pool).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(count = n, "marked interrupted task(s) as failed"),
        Err(e) => tracing::warn!(error = %e, "failed to reconcile interrupted tasks"),
    }

    let token = Token::load_or_create(&token_path)?;
    tracing::info!(
        data_dir = %data_dir.display(),
        "database at {}, token at {}",
        db_path.display(),
        token_path.display()
    );

    let bus = event_bus::EventBus::new(1024);
    let catalog = Arc::new(std::sync::RwLock::new(crate::tasks::load_catalog(
        &tasks_dir,
    )?));
    let mut agents = crate::agents::AgentManager::new();
    // Resolve every configured agent before wiring state: an unknown `type`
    // fails startup rather than the first launch.
    let registry = crate::agents::AgentRegistry::from_config(&config.read().unwrap())?;
    // Resolve `[git]` (global + per-agent) and materialize any generated scripts
    // once, so a malformed section fails startup like an unknown agent type.
    agents.configure_git(&config.read().unwrap(), data_dir.clone())?;
    let scheduler = tokio_cron_scheduler::JobScheduler::new().await?;

    // Resolve webhook secrets from config/env and validate the trigger rules before
    // starting up, so a bad rule (unknown event/task/glob) fails fast.
    let webhooks = WebhookSecrets::from_config(&config.read().unwrap());
    crate::webhooks::validate_rules(&config.read().unwrap(), &catalog.read().unwrap())?;

    // Notification hooks start empty; the TUI adds them live via `hooks.upsert`.
    let hook_store = Arc::new(std::sync::RwLock::new(Vec::new()));

    let state = Arc::new(State::new(
        pool,
        bus,
        token,
        webhooks,
        agents,
        registry,
        config,
        data_dir.clone(),
        tasks_dir,
        catalog,
        scheduler,
        hook_store.clone(),
    ));

    // Start the scheduler, executor, and hook engine, then register catalog
    // schedules (recurring tasks) with the scheduler.
    crate::scheduler::start(&state).await?;
    crate::executor::spawn(state.clone());
    hooks::HookEngine::new(hook_store, state.clone()).spawn();
    crate::scheduler::reconcile_catalog_schedules(&state).await?;

    // Reload the catalog when task files change on disk. A missing tasks dir
    // degrades gracefully: the rest of the daemon still runs.
    if let Err(e) = crate::catalog_watch::spawn(state.clone()) {
        tracing::warn!(error = %e, dir = %state.tasks_dir.display(), "failed to watch task catalog");
    }

    let unix_state = state.clone();
    let unix_task = tokio::spawn(async move { transport::serve_unix(&socket, unix_state).await });

    let app = Router::new()
        .route("/rpc", get(transport::ws_handler))
        .route("/metrics", get(crate::metrics::metrics_handler))
        .merge(crate::webhooks::routes())
        .merge(crate::pair::routes())
        .with_state(state.clone());
    let http_task = tokio::spawn(async move { transport::serve_http(&listen, app).await });

    tracing::info!("daemon started (ctrl-c to stop)");

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutdown signal received");
        }
        r = unix_task => {
            r??;
        }
        r = http_task => {
            r??;
        }
    }

    tracing::info!("daemon stopped");
    Ok(())
}
