//! `favetto-tui` — the favetto terminal client.
//!
//! This crate is deliberately dependency-light: it links only the shared wire
//! types from `favetto-core` (plus `favetto-providers` for the model catalog)
//! and its own TUI stack. It never pulls in the daemon's `axum`/`sqlx`/PTY
//! dependencies, so the client builds (and ships) independently of the daemon.

pub mod cli;
pub mod client;
pub mod supervisor;
pub mod tui;

// Shared types live in `favetto-core`; re-export them here so the TUI modules
// keep their `crate::{config,tasks,…}` paths.
pub use cli::TuiArgs;
pub use favetto_core::{config, paths, tasks, workflow, ws};
