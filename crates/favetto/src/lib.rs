//! `favetto` — LLM-driven agent orchestrator (daemon + remote TUI).
//!
//! This library exposes the crate's modules so the `favetto` binary and future
//! consumers (integration tests, a split daemon/TUI) can reuse them. The public
//! surface is deliberately small: the subcommand entry points plus the modules
//! shared between the daemon and the TUI client. The TUI client itself lives in
//! the separate, dependency-light `favetto-tui` crate.

// Panic-safety policy for the daemon's production code (issue #207): a reachable
// panic in the request loop can drop every attached TUI session. The restriction
// lints below are scoped to the non-test build so the crate's inline/dedicated
// test modules keep using `unwrap()`/`expect()` freely; see
// `docs/design/panic-safety.md`. Sites that are provably infallible carry a
// targeted `#[allow(..., reason = ...)]`.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented
    )
)]

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
mod ticket;
mod transport;
mod web;
mod webhooks;
