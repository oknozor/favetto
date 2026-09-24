//! `favetto` — LLM-driven agent orchestrator (daemon + remote TUI).
//!
//! This library exposes the crate's modules so the `favetto` binary and future
//! consumers (integration tests, a split daemon/TUI) can reuse them. The public
//! surface is deliberately small: the subcommand entry points plus the modules
//! shared between the daemon and the TUI client. Daemon internals stay private.

// Subcommand entry points.
pub mod cli;
pub mod daemon;
pub mod docgen;
pub mod tui;

pub mod pair;

// Shared pure modules now live in `favetto-core`; re-exported here so the
// daemon's internal `crate::<module>` paths keep resolving unchanged.
pub use favetto_core::{config, paths, tasks, template, workflow, ws};

// Daemon internals — not part of the reusable surface.
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
