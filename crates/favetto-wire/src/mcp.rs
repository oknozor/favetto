//! MCP server registry wire types.
//!
//! These are the *client-facing* definitions of MCP servers that Favetto passes
//! to an ACP agent. Favetto never installs, downloads or supervises them: a
//! definition only records how to launch (stdio) or reach (HTTP/SSE) a server
//! the user already installed.
//!
//! Secret env/header values are declared with a [`McpSecretSource`], never
//! returned in clear text. A literal value is accepted on write (`value`) and
//! stored by the daemon in its `0600` secrets file; read paths always clear it
//! and report [`McpSecret::has_value`] instead.

use std::collections::BTreeMap;
use std::fmt;

use crate::Timestamp;
use serde::{Deserialize, Serialize};

/// How Favetto connects to an MCP server.
///
/// Stdio is mandatory for every ACP agent; HTTP and SSE are opt-in through the
/// agent's advertised `mcpCapabilities`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    /// A child process spoken to over stdin/stdout.
    #[default]
    Stdio,
    /// Streamable HTTP.
    Http,
    /// Server-sent events.
    Sse,
}

impl McpTransport {
    /// The stable lower-case name used by the database and diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Http => "http",
            Self::Sse => "sse",
        }
    }

    /// Parse the database representation; unknown values fall back to stdio.
    pub fn parse(value: &str) -> Self {
        match value {
            "http" => Self::Http,
            "sse" => Self::Sse,
            _ => Self::Stdio,
        }
    }
}

/// Where a secret env/header value comes from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum McpSecretSource {
    /// Read the daemon environment variable named by [`McpSecret::env`].
    #[default]
    Env,
    /// Run [`McpSecret::command`] and use its trimmed stdout.
    Command,
    /// A literal value stored write-only in the daemon's `0600` secrets file.
    Literal,
}

/// One secret-bearing environment variable or HTTP header.
///
/// `value` is accepted on write and deliberately never serialized back: the
/// read path clears it and sets [`Self::has_value`]. `Debug` is manual for the
/// same reason.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpSecret {
    /// Environment variable name (stdio) or HTTP header name (HTTP/SSE).
    pub name: String,
    /// Where the value comes from. Defaults to the daemon environment.
    #[serde(default)]
    pub source: McpSecretSource,
    /// Source environment variable when `source = "env"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    /// Argv whose stdout is the value when `source = "command"`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// Write-only literal value when `source = "literal"`. Never serialized:
    /// read paths return only [`Self::has_value`].
    #[serde(default, skip_serializing)]
    pub value: Option<String>,
    /// Read-side marker: a literal value is stored for this entry.
    #[serde(default)]
    pub has_value: bool,
}

impl McpSecret {
    /// Build a declaration with no literal value.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            source: McpSecretSource::Env,
            env: None,
            command: Vec::new(),
            value: None,
            has_value: false,
        }
    }

    /// Force the literal source and clear the other source fields.
    pub fn with_literal(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            source: McpSecretSource::Literal,
            env: None,
            command: Vec::new(),
            value: Some(value.into()),
            has_value: true,
        }
    }
}

impl fmt::Debug for McpSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpSecret")
            .field("name", &self.name)
            .field("source", &self.source)
            .field("env", &self.env)
            .field("command", &self.command)
            .field("has_value", &(self.has_value || self.value.is_some()))
            .finish()
    }
}

/// Create/update payload for one MCP server definition.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpServerInput {
    pub name: String,
    #[serde(default)]
    pub transport: McpTransport,
    /// Stdio command. A bare name is resolved on the daemon's `PATH` and stored
    /// as an absolute path (ACP requires one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<McpSecret>,
    /// HTTP/SSE endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: Vec<McpSecret>,
    /// Whether the definition may be used. `None` keeps the stored value on
    /// update and defaults to `true` on add.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// A redacted MCP server definition as returned by `mcp.list` / `mcp.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpServerInfo {
    pub name: String,
    pub transport: McpTransport,
    /// Absolute stdio executable path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<McpSecret>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<McpSecret>,
    pub enabled: bool,
    /// Whether a stdio command may be launched. Editing the command/args clears
    /// this until the operator approves the new fingerprint.
    pub approved: bool,
    /// SHA-256 over the launch-relevant definition (never secret values).
    pub fingerprint: String,
    /// How the entry entered the registry.
    #[serde(default)]
    pub source: McpSource,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// How an MCP definition entered the registry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum McpSource {
    /// Added through `mcp.add` or the app.
    #[default]
    Manual,
    /// Seeded from `[[mcp.servers]]`; operator-authored and pre-approved.
    Config,
}

impl McpSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Config => "config",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "config" => Self::Config,
            _ => Self::Manual,
        }
    }
}

/// What one agent session actually received from the MCP registry.
///
/// `selected` is the resolved request (before per-transport capability
/// filtering), `active` is what was forwarded on the ACP session request, and
/// `warnings` explains anything that was omitted. Additive on the wire: older
/// clients ignore it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpSessionState {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selected: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Resolved secret values for one definition.
///
/// Deliberately not serializable: it exists only between the registry and the
/// ACP request builder, never in an RPC payload or log record.
#[derive(Default)]
pub struct McpResolvedSecrets {
    pub env: BTreeMap<String, String>,
    pub headers: BTreeMap<String, String>,
}

impl fmt::Debug for McpResolvedSecrets {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpResolvedSecrets")
            .field("env_names", &self.env.keys().collect::<Vec<_>>())
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_values_never_serialize_or_debug() {
        let secret = McpSecret::with_literal("TOKEN", "super-secret");
        let json = serde_json::to_value(&secret).unwrap();
        assert_eq!(json["name"], "TOKEN");
        assert!(json.get("value").is_none());

        let debug = format!("{secret:?}");
        assert!(!debug.contains("super-secret"), "{debug}");
        assert!(debug.contains("has_value"));
    }

    #[test]
    fn transport_round_trips_through_its_stable_name() {
        for transport in [McpTransport::Stdio, McpTransport::Http, McpTransport::Sse] {
            assert_eq!(McpTransport::parse(transport.as_str()), transport);
        }
        assert_eq!(McpTransport::parse("unknown"), McpTransport::Stdio);
    }

    #[test]
    fn session_state_omits_empty_fields() {
        let value = serde_json::to_value(McpSessionState::default()).unwrap();
        assert_eq!(value, serde_json::json!({}));
    }
}
