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

#[cfg(target_os = "android")]
mod android_notify;
pub mod app;
pub mod client;
pub mod components;
pub mod config;
#[cfg(not(target_arch = "wasm32"))]
pub mod http;
pub mod notifications;
pub mod sse;
pub mod state;
#[cfg(not(target_arch = "wasm32"))]
pub mod storage;
pub mod views;
