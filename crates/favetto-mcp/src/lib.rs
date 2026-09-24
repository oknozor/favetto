//! `favetto-mcp` — a Model Context Protocol supervisor for favetto workflows.
//!
//! The server is an ordinary remote-API client: it links only the shared wire
//! types from `favetto-core` and the `favetto-tui` [`Client`], never daemon
//! code. It exposes the **closed supervisor vocabulary** (see
//! `docs/reference/supervisor-contract.md`) as typed MCP tools, resources and a
//! prompt, and holds no policy of its own: the MCP client (an LLM) decides, the
//! daemon executes.
//!
//! The wire protocol is hand-rolled JSON-RPC 2.0 over stdio — one message per
//! line — implementing the `2025-06-18` MCP lifecycle subset an MCP client uses
//! for this feature: `initialize`, `tools/*`, `resources/*` and `prompts/*`.
//!
//! [`Client`]: favetto_tui::client::Client

pub mod prompts;
pub mod protocol;
pub mod resources;
pub mod rpc;
pub mod server;
pub mod tools;
