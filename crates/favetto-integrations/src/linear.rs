//! Thin Linear GraphQL client over `reqwest`.
//!
//! Only the operations and fields the favetto uses are typed; everything else
//! goes through the `raw` escape hatch. Auth is the standard Linear convention:
//! `Authorization: <api-key>` (no `Bearer` prefix).

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum LinearError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("graphql errors: {0}")]
    Graphql(Value),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Team {
    pub id: String,
    pub name: String,
    pub key: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IssueState {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Issue {
    pub id: String,
    pub identifier: String,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub state: Option<IssueState>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Comment {
    pub id: String,
    pub body: String,
}

#[derive(Clone)]
pub struct LinearClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl LinearClient {
    /// `base_url` is the GraphQL endpoint (e.g. `https://api.linear.app/graphql`,
    /// or the local mock `http://localhost:4000/graphql`).
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into(),
            api_key: api_key.into(),
        }
    }

    pub async fn list_teams(&self) -> Result<Vec<Team>, LinearError> {
        #[derive(Deserialize)]
        struct Data {
            teams: TeamConnection,
        }
        #[derive(Deserialize)]
        struct TeamConnection {
            nodes: Vec<Team>,
        }
        let data: Data = self
            .graphql("query { teams { nodes { id name key } } }", Value::Null)
            .await?;
        Ok(data.teams.nodes)
    }

    pub async fn list_issues(&self) -> Result<Vec<Issue>, LinearError> {
        #[derive(Deserialize)]
        struct Data {
            issues: IssueConnection,
        }
        #[derive(Deserialize)]
        struct IssueConnection {
            nodes: Vec<Issue>,
        }
        let data: Data = self
            .graphql("query { issues { nodes { id identifier title description state { name } } } }", Value::Null)
            .await?;
        Ok(data.issues.nodes)
    }

    pub async fn create_issue(
        &self,
        title: &str,
        team_id: &str,
        description: Option<&str>,
    ) -> Result<Issue, LinearError> {
        #[derive(Deserialize)]
        struct Data {
            issue_create: IssueCreate,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct IssueCreate {
            issue: Issue,
        }
        let variables = serde_json::json!({
            "input": {
                "title": title,
                "teamId": team_id,
                "description": description.unwrap_or_default(),
            }
        });
        let data: Data = self
            .graphql(
                "mutation($input: IssueCreateInput!) { issueCreate(input: $input) { issue { id identifier title description state { name } } } }",
                variables,
            )
            .await?;
        Ok(data.issue_create.issue)
    }

    pub async fn comment_issue(&self, issue_id: &str, body: &str) -> Result<Comment, LinearError> {
        #[derive(Deserialize)]
        struct Data {
            comment_create: CommentCreate,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct CommentCreate {
            comment: Comment,
        }
        let variables = serde_json::json!({
            "input": { "issueId": issue_id, "body": body }
        });
        let data: Data = self
            .graphql(
                "mutation($input: CommentCreateInput!) { commentCreate(input: $input) { comment { id body } } }",
                variables,
            )
            .await?;
        Ok(data.comment_create.comment)
    }

    /// Post a GraphQL document and decode the `data` object into `D`.
    async fn graphql<D: DeserializeOwned>(
        &self,
        query: &str,
        variables: Value,
    ) -> Result<D, LinearError> {
        let resp = self
            .http
            .post(&self.base_url)
            .header("Authorization", &self.api_key)
            .json(&serde_json::json!({ "query": query, "variables": variables }))
            .send()
            .await?
            .error_for_status()?;

        let body: Value = resp.json().await?;
        if let Some(errs) = body.get("errors") {
            return Err(LinearError::Graphql(errs.clone()));
        }
        let data = body.get("data").cloned().unwrap_or(Value::Null);
        Ok(serde_json::from_value(data)?)
    }
}
