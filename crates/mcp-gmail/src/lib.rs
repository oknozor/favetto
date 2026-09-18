//! `mcp-gmail` — first-party MCP server exposing Gmail operations.
//!
//! Configuration via environment:
//! - `GMAIL_ACCESS_TOKEN` (static) or `GMAIL_CLIENT_ID` + `GMAIL_CLIENT_SECRET` +
//!   `GMAIL_REFRESH_TOKEN` (OAuth refresh) — see `favetto-integrations::gmail::Auth`.
//! - `GMAIL_BASE_URL` (default `https://gmail.googleapis.com/gmail/v1`; point at the
//!   docker-compose mock for offline dev).

use anyhow::Context;
use favetto_integrations::gmail::{Auth, GmailClient};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const DEFAULT_BASE_URL: &str = "https://gmail.googleapis.com/gmail/v1";

#[derive(Clone)]
struct GmailServer {
    client: GmailClient,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct ListMessagesArgs {
    #[serde(default)]
    query: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct IdArgs {
    id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct SendArgs {
    to: String,
    subject: String,
    body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct ModifyLabelsArgs {
    id: String,
    #[serde(default)]
    add_label_ids: Vec<String>,
    #[serde(default)]
    remove_label_ids: Vec<String>,
}

#[tool_router]
impl GmailServer {
    #[tool(description = "List Gmail labels.")]
    async fn list_labels(&self) -> String {
        match self.client.list_labels().await {
            Ok(labels) => serde_json::json!({ "labels": labels }).to_string(),
            Err(e) => serde_json::json!({ "error": e.to_string() }).to_string(),
        }
    }

    #[tool(description = "List Gmail messages (metadata only), optionally filtered by a query.")]
    async fn list_messages(&self, Parameters(p): Parameters<ListMessagesArgs>) -> String {
        match self.client.list_messages(p.query.as_deref(), None).await {
            Ok(result) => serde_json::json!({ "result": result }).to_string(),
            Err(e) => serde_json::json!({ "error": e.to_string() }).to_string(),
        }
    }

    #[tool(description = "Fetch a full Gmail message by id, including decoded body text.")]
    async fn get_message(&self, Parameters(p): Parameters<IdArgs>) -> String {
        match self.client.get_message(&p.id).await {
            Ok(message) => serde_json::json!({
                "id": message.id,
                "thread_id": message.thread_id,
                "snippet": message.snippet,
                "subject": message.subject(),
                "from": message.from(),
                "labels": message.label_ids,
                "body": message.body_text(),
            })
            .to_string(),
            Err(e) => serde_json::json!({ "error": e.to_string() }).to_string(),
        }
    }

    #[tool(description = "Fetch a Gmail thread by id.")]
    async fn get_thread(&self, Parameters(p): Parameters<IdArgs>) -> String {
        match self.client.get_thread(&p.id).await {
            Ok(thread) => serde_json::json!({ "thread": thread }).to_string(),
            Err(e) => serde_json::json!({ "error": e.to_string() }).to_string(),
        }
    }

    #[tool(description = "Send a plain-text email.")]
    async fn send_message(&self, Parameters(p): Parameters<SendArgs>) -> String {
        match self
            .client
            .send_message(&p.to, &p.subject, &p.body)
            .await
        {
            Ok(sent) => serde_json::json!({ "sent": sent }).to_string(),
            Err(e) => serde_json::json!({ "error": e.to_string() }).to_string(),
        }
    }

    #[tool(description = "Add and/or remove labels on a message.")]
    async fn modify_labels(&self, Parameters(p): Parameters<ModifyLabelsArgs>) -> String {
        match self
            .client
            .modify_labels(&p.id, &p.add_label_ids, &p.remove_label_ids)
            .await
        {
            Ok(message) => serde_json::json!({ "message": message }).to_string(),
            Err(e) => serde_json::json!({ "error": e.to_string() }).to_string(),
        }
    }
}

#[tool_handler(
    name = "mcp-gmail",
    version = "0.1.0",
    instructions = "Read and send Gmail messages."
)]
impl ServerHandler for GmailServer {}

/// Run the server over stdio. `args` are unused (Gmail is configured via env).
pub async fn serve(_args: Vec<String>) -> anyhow::Result<()> {
    let auth = Auth::from_env().context(
        "set GMAIL_ACCESS_TOKEN, or GMAIL_CLIENT_ID + GMAIL_CLIENT_SECRET + GMAIL_REFRESH_TOKEN",
    )?;
    let base_url = std::env::var("GMAIL_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());

    let server = GmailServer {
        client: GmailClient::new(base_url, auth),
    };

    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
