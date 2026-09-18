//! `favetto-integrations` — thin API clients for the external services favetto drives.
//!
//! These are deliberately small, hand-rolled wrappers (no code-gen), mirroring the
//! project's "thin reqwest" philosophy:
//! - [`linear`]: a minimal GraphQL client over `reqwest` with typed structs for the
//!   subset of the Linear schema favetto uses.
//! - [`github`]: a thin wrapper over `octocrab`, with a base-URL override so it can
//!   target a mock for offline development.
//! - [`gmail`]: a thin `reqwest` client over the Gmail REST API (no `google-gmail1`),
//!   with static-token and OAuth-refresh auth and raw-MIME send support.
//!
//! Each is consumed by the corresponding first-party MCP server (`mcp-linear`,
//! `mcp-github`, `mcp-gmail`) and later by the daemon's webhook/action machinery.

pub mod github;
pub mod gmail;
pub mod linear;

