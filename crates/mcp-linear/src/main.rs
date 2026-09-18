//! `mcp-linear` — first-party MCP server exposing Linear issue/team operations.
//!
//! Configuration via environment:
//! - `LINEAR_API_KEY` (required; any non-empty value works against the local mock)
//! - `LINEAR_BASE_URL` (default `https://api.linear.app/graphql`; point at the
//!   docker-compose mock for offline dev)

use anyhow::Context;
use favetto_integrations::linear::{LinearClient, Team};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone)]
struct LinearServer {
    client: LinearClient,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct CreateIssueArgs {
    /// Issue title.
    title: String,
    /// Team id (obtain from list_teams).
    team_id: String,
    /// Optional Markdown description.
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct CommentIssueArgs {
    /// Issue id to comment on.
    issue_id: String,
    /// Comment body (Markdown).
    body: String,
}

#[tool_router]
impl LinearServer {
    #[tool(description = "List Linear teams.")]
    async fn list_teams(&self) -> String {
        match self.client.list_teams().await {
            Ok(teams) => {
                let teams: Vec<Team> = teams;
                serde_json::json!({ "teams": teams }).to_string()
            }
            Err(e) => serde_json::json!({ "error": e.to_string() }).to_string(),
        }
    }

    #[tool(description = "List Linear issues.")]
    async fn list_issues(&self) -> String {
        match self.client.list_issues().await {
            Ok(issues) => serde_json::json!({ "issues": issues }).to_string(),
            Err(e) => serde_json::json!({ "error": e.to_string() }).to_string(),
        }
    }

    #[tool(description = "Create a Linear issue in the given team.")]
    async fn create_issue(&self, Parameters(p): Parameters<CreateIssueArgs>) -> String {
        match self
            .client
            .create_issue(&p.title, &p.team_id, p.description.as_deref())
            .await
        {
            Ok(issue) => serde_json::json!({ "issue": issue }).to_string(),
            Err(e) => serde_json::json!({ "error": e.to_string() }).to_string(),
        }
    }

    #[tool(description = "Add a comment to a Linear issue.")]
    async fn comment_issue(&self, Parameters(p): Parameters<CommentIssueArgs>) -> String {
        match self.client.comment_issue(&p.issue_id, &p.body).await {
            Ok(comment) => serde_json::json!({ "comment": comment }).to_string(),
            Err(e) => serde_json::json!({ "error": e.to_string() }).to_string(),
        }
    }
}

#[tool_handler(
    name = "mcp-linear",
    version = "0.1.0",
    instructions = "Manage Linear issues and teams."
)]
impl ServerHandler for LinearServer {}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let api_key = std::env::var("LINEAR_API_KEY").context("LINEAR_API_KEY is not set")?;
    let base_url = std::env::var("LINEAR_BASE_URL")
        .unwrap_or_else(|_| "https://api.linear.app/graphql".to_string());

    let server = LinearServer {
        client: LinearClient::new(base_url, api_key),
    };

    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
