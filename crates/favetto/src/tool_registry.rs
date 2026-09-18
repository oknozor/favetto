//! ToolRegistry: the agent runtime's single query interface over every connected
//! MCP server.
//!
//! Every tool carries provenance (`server_id`, plus the server's tool metadata) so
//! logs, permissions, and the LLM prompt can all be traced back to a source. The
//! per-skill allowlist is enforced here: a skill only ever sees/calls the
//! `(server, tool)` pairs its `config.toml` declares.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Context;
use rmcp::model::Tool;

use crate::mcp_client::McpSession;

/// A tool plus the server that provides it.
#[derive(Clone)]
pub struct ToolRef {
    pub server_id: String,
    pub tool: Tool,
}

impl ToolRef {
    pub fn name(&self) -> &str {
        &self.tool.name
    }
}

/// Aggregates sessions and mediates tool access.
pub struct ToolRegistry {
    sessions: BTreeMap<String, Arc<McpSession>>,
}

impl ToolRegistry {
    pub fn new(sessions: Vec<McpSession>) -> Self {
        let map = sessions
            .into_iter()
            .map(|s| (s.name.clone(), Arc::new(s)))
            .collect();
        Self { sessions: map }
    }

    /// Names of all connected servers (drives the MCP TUI tab in a later milestone).
    #[allow(dead_code)]
    pub fn server_names(&self) -> Vec<&str> {
        self.sessions.keys().map(|s| s.as_str()).collect()
    }

    /// Every tool across every server.
    #[allow(dead_code)]
    pub fn all_tools(&self) -> Vec<ToolRef> {
        self.sessions
            .iter()
            .flat_map(|(name, s)| {
                s.tools
                    .iter()
                    .cloned()
                    .map(|tool| ToolRef {
                        server_id: name.clone(),
                        tool,
                    })
            })
            .collect()
    }

    /// Tools permitted by a skill's allowlist (`server → tool names`). An empty
    /// allowlist grants nothing — access is deny-by-default.
    pub fn allowlisted(&self, allow: &BTreeMap<String, Vec<String>>) -> Vec<ToolRef> {
        let mut out = Vec::new();
        for (server_id, allowed_tools) in allow {
            let Some(session) = self.sessions.get(server_id) else {
                continue;
            };
            for tool in &session.tools {
                if allowed_tools.iter().any(|a| a == tool.name.as_ref()) {
                    out.push(ToolRef {
                        server_id: server_id.clone(),
                        tool: tool.clone(),
                    });
                }
            }
        }
        out
    }

    /// Call a tool by server and name, returning its textual result for the LLM loop.
    pub async fn call(
        &self,
        server: &str,
        tool: &str,
        args: serde_json::Value,
    ) -> anyhow::Result<String> {
        let started = std::time::Instant::now();

        let session = self
            .sessions
            .get(server)
            .with_context(|| format!("no MCP server named '{server}'"))?;

        let arg_map = match args {
            serde_json::Value::Object(m) => m,
            serde_json::Value::Null => serde_json::Map::new(),
            other => anyhow::bail!("tool arguments must be an object, got {other}"),
        };

        let result = session.call_tool(tool, arg_map).await;
        crate::metrics::record_tool_call(
            server,
            tool,
            started.elapsed().as_millis() as u64,
            result.is_ok(),
        );
        let result = result?;

        let mut text = result
            .content
            .iter()
            .filter_map(|c| c.as_text())
            .map(|t| t.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        if text.is_empty() {
            if let Some(sc) = &result.structured_content {
                text = sc.to_string();
            }
        }

        if result.is_error == Some(true) {
            anyhow::bail!("tool {server}.{tool} returned an error: {text}");
        }

        Ok(text)
    }
}
