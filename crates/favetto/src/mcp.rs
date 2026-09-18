//! MCP server configuration types, shared by the global config (`[mcp.*]`) and the
//! MCP client pool.

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// How an MCP server is reached.
#[derive(Debug, Clone)]
pub enum McpTransport {
    /// Spawn a child process (e.g. `mcp-filesystem --root ./workdir`).
    Stdio { command: String, args: Vec<String> },
    /// Connect to a remote MCP server. Deferred.
    StreamableHttp {
        #[allow(dead_code)] // consumed once the HTTP transport lands
        url: String,
    },
}

/// A normalized MCP server declaration.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: McpTransport,
}

/// The `[mcp.<name>]` table as declared in the config file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerToml {
    pub transport: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub url: Option<String>,
}

/// Convert an `[mcp.<name>]` table into a normalized [`McpServerConfig`].
pub fn server_config(owner: &str, name: &str, toml: &McpServerToml) -> anyhow::Result<McpServerConfig> {
    match toml.transport.as_str() {
        "stdio" => {
            let command = toml
                .command
                .clone()
                .with_context(|| format!("{owner}: mcp.{name} missing 'command'"))?;
            Ok(McpServerConfig {
                name: name.to_string(),
                transport: McpTransport::Stdio {
                    command,
                    args: toml.args.clone().unwrap_or_default(),
                },
            })
        }
        "streamable_http" => {
            let url = toml
                .url
                .clone()
                .with_context(|| format!("{owner}: mcp.{name} missing 'url'"))?;
            Ok(McpServerConfig {
                name: name.to_string(),
                transport: McpTransport::StreamableHttp { url },
            })
        }
        other => anyhow::bail!("{owner}: mcp.{name} has unknown transport {other:?}"),
    }
}
