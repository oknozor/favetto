//! `mcp-github` — first-party MCP server exposing GitHub issue/PR operations.
//!
//! Configuration via environment:
//! - `GITHUB_TOKEN` (required)
//! - `GITHUB_BASE_URL` (optional; overrides the API root for offline/mock dev)

use anyhow::Context;
use favetto_integrations::github::GitHubClient;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone)]
struct GitHubServer {
    client: GitHubClient,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct RepoArgs {
    /// Repository as `owner/repo`.
    repo: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct CreateIssueArgs {
    repo: String,
    title: String,
    #[serde(default)]
    body: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct CommentIssueArgs {
    repo: String,
    /// Issue or PR number.
    issue_number: u64,
    body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct OpenPrArgs {
    repo: String,
    title: String,
    /// Branch with the changes.
    head: String,
    /// Branch to merge into.
    base: String,
    #[serde(default)]
    body: Option<String>,
}

#[tool_router]
impl GitHubServer {
    #[tool(description = "List open issues in a repository.")]
    async fn list_issues(&self, Parameters(p): Parameters<RepoArgs>) -> String {
        match self.client.list_issues(&p.repo).await {
            Ok(issues) => serde_json::json!({ "issues": issues }).to_string(),
            Err(e) => serde_json::json!({ "error": e.to_string() }).to_string(),
        }
    }

    #[tool(description = "Create an issue in a repository.")]
    async fn create_issue(&self, Parameters(p): Parameters<CreateIssueArgs>) -> String {
        match self.client.create_issue(&p.repo, &p.title, p.body.as_deref()).await {
            Ok(issue) => serde_json::json!({ "issue": issue }).to_string(),
            Err(e) => serde_json::json!({ "error": e.to_string() }).to_string(),
        }
    }

    #[tool(description = "Comment on an issue or PR.")]
    async fn comment_issue(&self, Parameters(p): Parameters<CommentIssueArgs>) -> String {
        match self
            .client
            .comment_issue(&p.repo, p.issue_number, &p.body)
            .await
        {
            Ok(comment) => serde_json::json!({ "comment": comment }).to_string(),
            Err(e) => serde_json::json!({ "error": e.to_string() }).to_string(),
        }
    }

    #[tool(description = "Open a pull request.")]
    async fn open_pr(&self, Parameters(p): Parameters<OpenPrArgs>) -> String {
        match self
            .client
            .open_pr(&p.repo, &p.title, &p.head, &p.base, p.body.as_deref())
            .await
        {
            Ok(pr) => serde_json::json!({ "pull_request": pr }).to_string(),
            Err(e) => serde_json::json!({ "error": e.to_string() }).to_string(),
        }
    }
}

#[tool_handler(
    name = "mcp-github",
    version = "0.1.0",
    instructions = "Manage GitHub issues and pull requests."
)]
impl ServerHandler for GitHubServer {}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let token = std::env::var("GITHUB_TOKEN").context("GITHUB_TOKEN is not set")?;
    let base_url = std::env::var("GITHUB_BASE_URL").ok();

    let server = GitHubServer {
        client: GitHubClient::new(&token, base_url.as_deref())?,
    };

    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
