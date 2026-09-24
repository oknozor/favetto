//! `favetto-core` — shared types and wire protocol for the favetto.
//!
//! This crate is the contract between the daemon and the TUI client. It is
//! deliberately dependency-light: no HTTP, no database, no LLM — just the shared
//! vocabulary, configuration, task/workflow model, and wire framing everyone
//! agrees on.
//!
//! Modules:
//! - [`model`]: domain types (tasks, events) shared by daemon and TUI.
//! - [`agent_state`]: normalized external-agent state (activity, usage, events).
//! - [`rpc`]: JSON-RPC-style request/response/push types encoded as MessagePack.
//! - [`wire`]: length-prefixed MessagePack framing over raw byte streams.
//! - [`auth`]: bearer-token generation and constant-time verification.
//! - [`config`]: the global `config.toml` model, shared by daemon and TUI.
//! - [`tasks`]: the Markdown task catalog parser, shared by daemon and TUI.
//! - [`workflow`]: the derived `needs`/`spawn` graph, shared by daemon and TUI.
//! - [`paths`]: shared path helpers (`~` expansion, default locations).
//! - [`ws`]: the MessagePack-frame ↔ WebSocket payload conversion both ends use.

pub mod agent_state;
pub mod auth;
pub mod config;
pub mod model;
pub mod paths;
pub mod rpc;
pub mod tasks;
pub mod wire;
pub mod workflow;
pub mod ws;
