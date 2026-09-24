//! `favetto-core` — shared types and wire protocol for the favetto.
//!
//! This crate is the contract between the daemon and the TUI client. It is
//! deliberately dependency-light: no HTTP, no database, no LLM — just the
//! vocabulary and the framing everyone agrees on.
//!
//! Modules:
//! - [`model`]: domain types (tasks, events) shared by daemon and TUI.
//! - [`rpc`]: JSON-RPC-style request/response/push types encoded as MessagePack.
//! - [`wire`]: length-prefixed MessagePack framing over raw byte streams.
//! - [`auth`]: bearer-token generation and constant-time verification.
//! - [`config`]: the global `config.toml` types.
//! - [`paths`]: shared filesystem path helpers.
//! - [`tasks`]: the task catalog (Markdown + TOML front-matter).
//! - [`template`]: `{{ dotted.path }}` prompt rendering.
//! - [`workflow`]: the catalog's `needs`/`spawn` graph.
//! - [`ws`]: MessagePack-frame ↔ WebSocket payload conversion.

pub mod auth;
pub mod config;
pub mod model;
pub mod paths;
pub mod rpc;
pub mod tasks;
pub mod template;
pub mod wire;
pub mod workflow;
pub mod ws;
