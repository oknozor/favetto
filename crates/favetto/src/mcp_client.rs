//! MCP client: a single long-lived session to one MCP server, built on `rmcp`.
//!
//! M2 supports the stdio transport (spawning a child process); the streamable-HTTP
//! transport is declared in [`McpTransport`](crate::mcp::McpTransport) and lands
//! in a later milestone. A session owns its connection for its whole lifetime and
//! exposes the server's tool list plus a `call_tool` primitive.

use anyhow::Context;
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig, Tool};
use rmcp::transport::TokioChildProcess;
use rmcp::{ClientHandler, ServiceExt};

use crate::mcp::{McpServerConfig, McpTransport};

/// Minimal client-side handler. The client role only needs identity info.
#[derive(Clone)]
struct Client;

impl ClientHandler for Client {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::default()
    }
}

type Running = rmcp::service::RunningService<rmcp::RoleClient, Client>;

/// A live connection to one MCP server, plus its cached tool list.
pub struct McpSession {
    pub name: String,
    running: Running,
    pub tools: Vec<Tool>,
}

impl McpSession {
    /// Connect (and initialize) a session for `config`.
    pub async fn connect(config: &McpServerConfig) -> anyhow::Result<Self> {
        let running = match &config.transport {
            McpTransport::Stdio { command, args } => {
                let cmd = spawn_command(command, args);
                let transport = TokioChildProcess::new(cmd).with_context(|| {
                    format!(
                        "spawn MCP server '{command}' (built-in servers run in-process; external \
                         ones must be built and on PATH)"
                    )
                })?;
                Client
                    .serve(transport)
                    .await
                    .with_context(|| format!("initialize MCP server '{}'", config.name))?
            }
            McpTransport::StreamableHttp { .. } => {
                anyhow::bail!(
                    "MCP server '{}': streamable HTTP transport arrives in M3",
                    config.name
                )
            }
        };

        let tools = running
            .list_all_tools()
            .await
            .with_context(|| format!("list tools from MCP server '{}'", config.name))?;

        Ok(Self {
            name: config.name.clone(),
            running,
            tools,
        })
    }

    /// Call a tool, returning the raw MCP result.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        args: serde_json::Map<String, serde_json::Value>,
    ) -> anyhow::Result<CallToolResult> {
        let params = CallToolRequestParams::new(tool_name.to_string()).with_arguments(args);
        Ok(self.running.call_tool(params).await?)
    }

    /// Gracefully close the session.
    #[allow(dead_code)] // used by the pool's restart-on-crash logic (M3+)
    pub async fn shutdown(self) {
        let _ = self.running.cancel().await;
    }
}

/// Built-in MCP servers that favetto can run in-process (via `favetto __mcp`).
const BUILTIN_SERVERS: &[&str] = &["mcp-filesystem", "mcp-linear", "mcp-github", "mcp-gmail"];

/// Build the process command for an MCP server.
///
/// Built-in servers are run in-process by re-invoking the current `favetto` binary
/// with a hidden `__mcp <server>` subcommand, so they are always available even when
/// only `favetto` itself was installed. External servers resolve to `<exe_dir>/<name>`
/// first, then `PATH`.
fn spawn_command(command: &str, args: &[String]) -> tokio::process::Command {
    if BUILTIN_SERVERS.contains(&command) {
        if let Ok(exe) = std::env::current_exe() {
            let mut cmd = tokio::process::Command::new(exe);
            cmd.arg("__mcp").arg(command);
            cmd.args(args);
            return cmd;
        }
    }

    let mut cmd = tokio::process::Command::new(resolve_binary(command));
    cmd.args(args);
    cmd
}

/// Resolve a server binary: prefer `<exe_dir>/<command>` (so `cargo build` layouts
/// work without installing), then fall back to the `PATH`.
fn resolve_binary(command: &str) -> String {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(command);
            if candidate.exists() {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    command.to_string()
}
