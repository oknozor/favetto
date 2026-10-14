//! favetto web client.
//!
//! A client-only Dioxus application shared by two targets:
//!
//! - **web** (`wasm32-unknown-unknown`): built with `dx build --platform web`
//!   and served by the daemon from its embedded `web/dist` assets.
//! - **Android** (native): built with `dx build --platform android --features mobile`
//!   and connects to a remote daemon.
//!
//! The crate is a standalone cargo workspace (excluded from the root
//! workspace) so the native gates do not need the Dioxus toolchain.

pub mod app;
pub mod client;
pub mod config;
pub mod sse;
pub mod state;
