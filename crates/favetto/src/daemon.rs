//! The `daemon` subcommand: wire up persistence, the event bus, webhooks, hooks,
//! transports, and the synthetic driver, then run until a shutdown signal arrives.

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
    let config = Arc::new(match FavettoConfig::load_from(&config_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, path = %config_path.display(), "failed to load config; using defaults");
            FavettoConfig::default()
        }
    });

    // Resolve settings: CLI flag > config > default.
    let data_dir = args
        .data_dir
        .clone()
        .or_else(|| config.daemon.data_dir.clone())
        .unwrap_or_else(crate::cli::default_data_dir);
    let socket = args
        .socket
        .clone()
        .or_else(|| config.daemon.socket.clone())
        .unwrap_or_else(|| PathBuf::from("/tmp/favetto.sock"));
    let listen = args
        .listen
        .clone()
        .or_else(|| config.daemon.listen.clone())
        .unwrap_or_else(|| "127.0.0.1:7878".to_string());
    let skills_dir = args
        .skills_dir
        .clone()
        .or_else(|| config.daemon.skills_dir.clone())
        .unwrap_or_else(|| PathBuf::from("skills"));
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
    let chat = crate::chat::ChatManager::new(skills_dir.clone(), config.clone());
    let scheduler = tokio_cron_scheduler::JobScheduler::new().await?;

    let state = Arc::new(State::new(
        pool,
        bus,
        token,
        WebhookSecrets::from_env(),
        chat,
        config,
        skills_dir,
        scheduler,
    ));

    // Start the scheduler (persisted cron schedules) and the task queue executor.
    crate::scheduler::start(&state).await?;
    crate::executor::spawn(state.clone());

    // Load and start the hook engine.
    let hooks = hooks::load_hooks(&hooks_path)?;
    tracing::info!(count = hooks.len(), "loaded hooks");
    if !hooks.is_empty() {
        hooks::HookEngine::new(hooks, state.clone()).spawn();
    }

    if !args.no_synthetic {
        tokio::spawn(crate::synthetic::run(state.clone()));
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
