//! `favetto-integrations` — thin API clients for the external services the
//! favetto drives.
//!
//! These are deliberately small, hand-rolled wrappers (no code-gen), mirroring the
//! project's "thin reqwest" philosophy:
//! - [`linear`]: a minimal GraphQL client over `reqwest` with typed structs for the
//!   subset of the Linear schema the favetto uses.
//! - [`github`]: a thin wrapper over `octocrab`, with a base-URL override so it can
//!   target a mock for offline development.
//!
//! Both are consumed by the first-party MCP servers (`mcp-linear`, `mcp-github`) and
//! later by the daemon's webhook/action machinery.

pub mod github;
pub mod linear;
