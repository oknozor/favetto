//! `favetto` — LLM-driven agent orchestrator (daemon + remote TUI).
//!
//! This library exposes the crate's modules so the `favetto` binary and future
//! consumers (integration tests, a split daemon/TUI) can reuse them. The public
//! surface is deliberately small: the subcommand entry points plus the modules
//! shared between the daemon and the TUI client. The TUI client itself lives in
//! the separate, dependency-light `favetto-tui` crate.

// Subcommand entry points.
pub mod cli;
pub mod daemon;
pub mod docgen;
pub mod pair;

// Modules shared between the daemon and the TUI client live in `favetto-core`
// and are re-exported here so the daemon keeps its `crate::{config,tasks,…}`
// paths. The TUI crate depends on `favetto-core` directly.
pub mod template;
pub use favetto_core::{config, paths, tasks, workflow, ws};

// Daemon internals — not part of the reusable surface.
mod agent_hooks;
mod agents;
mod attention;
mod catalog_watch;
mod db;
mod event_bus;
mod executor;
mod git;
mod hooks;
mod metrics;
mod notify;
mod scheduler;
mod server;
mod state;
mod transport;
mod webhooks;
