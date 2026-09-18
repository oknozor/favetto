//! `mcp serve` — expose the orchestrator itself as an MCP server.
//!
//! Bridges to a running daemon over the wire protocol (Unix socket or WebSocket) and
//! presents its operations as MCP tools, so *other* agents can drive favetto:
//! `create_task`, `list_tasks`, `cancel_task`, `query_events`, `send_notification`.

use anyhow::Context;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cli::McpServeArgs;
use crate::client::{Client, Transport};

#[derive(Clone)]
struct OrchestratorMcp {
    client: Client,
}

impl OrchestratorMcp {
    async fn call(&self, method: &str, params: Value) -> String {
        match self.client.request(method, params).await {
            Ok(resp) => match resp.result {
                Some(v) => v.to_string(),
                None => format!("error: {:?}", resp.error),
            },
            Err(e) => format!("error: {e}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct CreateTask {
    /// Skill name to run.
    skill: String,
    /// Optional input JSON for the skill.
    #[serde(default)]
    input: Option<Value>,
    /// Optional dedupe key for idempotency.
    #[serde(default)]
    dedupe_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct IdArg {
    id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct QueryEvents {
    /// Max events to return.
    #[serde(default)]
    limit: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct SendNotification {
    channel: String,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    config: Option<Value>,
}

#[tool_router]
impl OrchestratorMcp {
    #[tool(description = "Create a task (enqueued for the agent runtime).")]
    async fn create_task(&self, Parameters(p): Parameters<CreateTask>) -> String {
        let params = serde_json::json!({
            "skill": p.skill,
            "input": p.input.unwrap_or(Value::Null),
            "dedupe_key": p.dedupe_key,
        });
        self.call("tasks.create", params).await
    }

    #[tool(description = "List tasks.")]
    async fn list_tasks(&self) -> String {
        self.call("tasks.list", serde_json::json!({})).await
    }

    #[tool(description = "Cancel a task by id.")]
    async fn cancel_task(&self, Parameters(p): Parameters<IdArg>) -> String {
        self.call("tasks.cancel", serde_json::json!({ "id": p.id })).await
    }

    #[tool(description = "Query recent events.")]
    async fn query_events(&self, Parameters(p): Parameters<QueryEvents>) -> String {
        let limit = p.limit.unwrap_or(50);
        self.call("events.tail", serde_json::json!({ "limit": limit })).await
    }

    #[tool(description = "Send a notification through a channel (log, webhook, ntfy).")]
    async fn send_notification(&self, Parameters(p): Parameters<SendNotification>) -> String {
        let params = serde_json::json!({
            "channel": p.channel,
            "subject": p.subject.unwrap_or_default(),
            "body": p.body.unwrap_or_default(),
            "config": p.config.unwrap_or(Value::Null),
        });
        self.call("notifications.test", params).await
    }
}

#[tool_handler(
    name = "favetto",
    version = "0.1.0",
    instructions = "Drive the favetto agent orchestrator: create/cancel tasks, query events, send notifications."
)]
impl ServerHandler for OrchestratorMcp {}

pub async fn run(args: McpServeArgs) -> anyhow::Result<()> {
    let transport = match args.remote {
        Some(url) => Transport::Ws {
            url,
            token: args.token,
        },
        None => Transport::Unix(args.socket),
    };

    let client = Client::connect(transport)
        .await
        .context("connect to favetto daemon")?;
    let server = OrchestratorMcp { client };

    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
