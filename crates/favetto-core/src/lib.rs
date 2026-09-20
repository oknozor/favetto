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

pub mod auth;
pub mod model;
pub mod rpc;
pub mod wire;
