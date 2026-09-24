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
use crate::state::{State, StateInit};
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

    // Resolve settings: CLI flag > config > default. The config is loaded once up
    // front and shared immutably from here on.
    let data_dir = crate::paths::expand_tilde({
        args.data_dir
            .clone()
            .or_else(|| config.daemon.data_dir.clone())
            .unwrap_or_else(crate::cli::default_data_dir)
    });
    let socket = {
        args.socket
            .clone()
            .or_else(|| config.daemon.socket.clone())
            .unwrap_or_else(|| PathBuf::from("/tmp/favetto.sock"))
    };
    let listen = {
        args.listen
            .clone()
            .or_else(|| config.daemon.listen.clone())
            .unwrap_or_else(|| "127.0.0.1:7878".to_string())
    };
    let tasks_dir = crate::paths::expand_tilde({
        args.tasks_dir
            .clone()
            .or_else(|| config.daemon.tasks_dir.clone())
            .unwrap_or_else(|| PathBuf::from("tasks"))
    });
    tokio::fs::create_dir_all(&data_dir).await?;

    let db_path = data_dir.join("favetto.db");
    let token_path = data_dir.join("token");

    let pool = db::open(&db_path).await?;
    db::migrate(&pool).await?;

    // Bounded growth: run one retention pass at startup. Never fatal.
    let retention = config.daemon.retention.clone();
    match db::prune(&pool, retention.days, retention.min_tasks, retention.vacuum).await {
        Ok(stats) if stats != db::PruneStats::default() => {
            tracing::info!(?stats, "pruned database")
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "database prune failed"),
    }

    let token = Token::load_or_create(&token_path)?;
    tracing::info!(
        data_dir = %data_dir.display(),
        "database at {}, token at {}",
        db_path.display(),
        token_path.display()
    );

    let bus = event_bus::EventBus::new(1024);
    let catalog = Arc::new(parking_lot::RwLock::new(crate::tasks::load_catalog(
        &tasks_dir,
    )?));
    let mut agents = crate::agents::AgentManager::new();
    // Resolve every configured agent before wiring state: an unknown `type`
    // fails startup rather than the first launch.
    let registry = crate::agents::AgentRegistry::from_config(&config)?;
    // Resolve `[git]` (global + per-agent) and materialize any generated scripts
    // once, so a malformed section fails startup like an unknown agent type.
    agents.configure_git(&config, data_dir.clone())?;
    // Claude sessions report live state over a per-launch loopback HTTP hook.
    // Hooks are always available in daemon mode; unit tests never configure them.
    agents.configure_hooks(&listen, data_dir.clone());
    let scheduler = tokio_cron_scheduler::JobScheduler::new().await?;

    // Resolve webhook secrets from config/env and validate the trigger rules before
    // starting up, so a bad rule (unknown event/task/glob) fails fast.
    let webhooks = WebhookSecrets::from_config(&config);
    crate::webhooks::validate_rules(&config, &catalog.read())?;

    // Notification hooks start empty; the TUI adds them live via `hooks.upsert`.
    let hook_store = Arc::new(parking_lot::RwLock::new(Vec::new()));

    let state = Arc::new(State::new(StateInit {
        db: pool,
        bus,
        token,
        webhooks,
        agents,
        registry,
        config,
        data_dir: data_dir.clone(),
        tasks_dir,
        catalog,
        scheduler,
        hook_store: hook_store.clone(),
    }));

    // Materialize the catalog graph once at startup so `<data_dir>/workflow.dot`
    // exists even before the first catalog change.
    if let Err(e) = crate::workflow::regenerate(&state.catalog.read(), &state.data_dir) {
        tracing::warn!(error = %e, "failed to write workflow.dot");
    }

    // Reconcile state left by a previous instance before anything consumes the
    // queue: interrupted runs become `interrupted`, stale tasks follow
    // `[executor].stale_run`, and pending tasks whose catalog definition vanished
    // are failed. Idempotent; never fatal.
    match crate::executor::reconcile(&state).await {
        Ok(report) if report != crate::executor::ReconcileReport::default() => {
            tracing::info!(?report, "reconciled interrupted runs")
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "startup reconcile failed"),
    }

    // Start the scheduler, executor, and hook engine, then register catalog
    // schedules (recurring tasks) with the scheduler.
    crate::scheduler::start(&state).await?;
    crate::executor::spawn(state.clone());
    hooks::HookEngine::new(hook_store, state.clone()).spawn();
    crate::scheduler::reconcile_catalog_schedules(&state).await?;

    // Reclaim worktrees left by finished tasks once at startup. Never fatal.
    match crate::executor::prune_worktrees(&state).await {
        Ok(stats) if stats != crate::executor::WorktreePruneStats::default() => {
            tracing::info!(?stats, "pruned worktrees");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "worktree prune failed"),
    }

    // Re-run retention every six hours so a long-lived daemon stays bounded.
    if retention.days > 0 {
        let pool = state.db.clone();
        let retention = retention.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(6 * 60 * 60));
            tick.tick().await; // consume the immediate first tick
            loop {
                tick.tick().await;
                if let Err(e) =
                    db::prune(&pool, retention.days, retention.min_tasks, retention.vacuum).await
                {
                    tracing::warn!(error = %e, "periodic database prune failed");
                }
            }
        });
    }

    // Re-run the worktree sweep every six hours as well.
    if state.config.executor.worktree {
        let state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(6 * 60 * 60));
            tick.tick().await; // consume the immediate first tick
            loop {
                tick.tick().await;
                if let Err(e) = crate::executor::prune_worktrees(&state).await {
                    tracing::warn!(error = %e, "periodic worktree prune failed");
                }
            }
        });
    }

    // Reload the catalog when task files change on disk. A missing tasks dir
    // degrades gracefully: the rest of the daemon still runs.
    if let Err(e) = crate::catalog_watch::spawn(state.clone()) {
        tracing::warn!(error = %e, dir = %state.tasks_dir.display(), "failed to watch task catalog");
    }

    let unix_state = state.clone();
    let unix_task = tokio::spawn(async move { transport::serve_unix(&socket, unix_state).await });

    let app = Router::new()
        .merge(transport::routes())
        .route("/metrics", get(crate::metrics::metrics_handler))
        .merge(crate::webhooks::routes())
        .merge(crate::agent_hooks::routes())
        .merge(crate::pair::routes())
        .merge(crate::ticket::routes())
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
