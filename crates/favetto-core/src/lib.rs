//! `favetto-core` — shared types and wire protocol for the favetto.
//!
//! This crate is the contract between the daemon and the TUI client. It is
//! deliberately dependency-light: no HTTP, no database, no LLM — just the shared
//! vocabulary, configuration, task/workflow model, and wire framing everyone
//! agrees on.
//!
//! The platform-free value types now live in `favetto-wire` and are re-exported
//! here so every existing import path keeps working:
//! - [`model`]: domain types (tasks, events) shared by daemon and TUI.
//! - [`agent_state`]: normalized external-agent state (activity, usage, events).
//! - [`rpc`]: JSON-RPC-style request/response/push types encoded as MessagePack.
//!
//! Modules:
//! - [`wire`]: length-prefixed MessagePack framing over raw byte streams.
//! - [`auth`]: bearer-token generation and constant-time verification.
//! - [`config`]: the global `config.toml` model, shared by daemon and TUI.
//! - [`tasks`]: the Markdown task catalog parser, shared by daemon and TUI.
//! - [`workflow`]: the derived `needs`/`spawn` graph, shared by daemon and TUI.
//! - [`paths`]: shared path helpers (`~` expansion, default locations).
//! - [`ws`]: the MessagePack-frame ↔ WebSocket payload conversion both ends use.

pub mod auth;
pub mod config;
pub mod paths;
pub mod tasks;
pub mod wire;
pub mod workflow;
pub mod ws;

// The platform-free wire/domain types now live in `favetto-wire`; re-export the
// modules so every `favetto_core::{model,agent_state,rpc}::…` import keeps working.
pub use favetto_wire::{agent_state, model, rpc};
