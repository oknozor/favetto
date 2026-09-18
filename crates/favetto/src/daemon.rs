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
    let config = Arc::new(std::sync::RwLock::new(match FavettoConfig::load_from(&config_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, path = %config_path.display(), "failed to load config; using defaults");
            FavettoConfig::default()
        }
    }));

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
    let hooks_path = args
        .hooks
        .clone()
        .unwrap_or_else(|| PathBuf::from("hooks.toml"));

    tokio::fs::create_dir_all(&data_dir).await?;

    let db_path = data_dir.join("favetto.db");
    let token_path = data_dir.join("token");

    let pool = db::open(&db_path).await?;
    db::migrate(&pool).await?;

    let token = Token::load_or_create(&token_path)?;
    tracing::info!(
        data_dir = %data_dir.display(),
        "database at {}, token at {}",
        db_path.display(),
        token_path.display()
    );

    let bus = event_bus::EventBus::new(1024);
    let catalog = Arc::new(std::sync::RwLock::new(
        crate::tasks::load_catalog(&tasks_dir)?,
    ));
    let chat = crate::chat::ChatManager::new(pool.clone(), config.clone(), catalog.clone());
    let scheduler = tokio_cron_scheduler::JobScheduler::new().await?;
    let hook_store = Arc::new(std::sync::RwLock::new(hooks::load_hooks(&hooks_path)?));

    let state = Arc::new(State::new(
        pool,
        bus,
        token,
        WebhookSecrets::from_env(),
        chat,
        config,
        config_path,
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
    crate::scheduler::sync_catalog_schedules(&state).await?;

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
