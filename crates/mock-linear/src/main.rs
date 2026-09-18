//! `mock-linear` — a tiny GraphQL mock of Linear's API for offline development.
//!
//! It implements only the operations the favetto's `LinearClient` uses
//! (`teams`, `issues`, `issueCreate`, `commentCreate`) and returns deterministic
//! data, echoing a few input fields so end-to-end flows are observable. Run
//! directly with `cargo run -p mock-linear`, or via `docker compose up`.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

const PORT: &str = "0.0.0.0:4000";

async fn graphql(Json(body): Json<Value>) -> Response {
    let query = body.get("query").and_then(|q| q.as_str()).unwrap_or("");
    let variables = body.get("variables").cloned().unwrap_or(Value::Null);

    let data = if query.contains("issueCreate") {
        let input = variables.get("input").cloned().unwrap_or(Value::Null);
        let title = input
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or("Untitled")
            .to_string();
        let team_id = input
            .get("teamId")
            .and_then(|t| t.as_str())
            .unwrap_or("team-eng")
            .to_string();
        let key = if team_id == "team-prod" { "PROD" } else { "ENG" };
        json!({
            "issueCreate": { "issue": {
                "id": "issue-new",
                "identifier": format!("{key}-2"),
                "title": title,
                "description": null,
                "state": { "name": "Todo" }
            }}
        })
    } else if query.contains("commentCreate") {
        let body_text = variables
            .get("input")
            .and_then(|i| i.get("body"))
            .and_then(|b| b.as_str())
            .unwrap_or("")
            .to_string();
        json!({
            "commentCreate": { "comment": { "id": "comment-1", "body": body_text } }
        })
    } else if query.contains("teams") {
        json!({
            "teams": { "nodes": [
                { "id": "team-eng", "name": "Engineering", "key": "ENG" },
                { "id": "team-prod", "name": "Product", "key": "PROD" }
            ]}
        })
    } else if query.contains("issues") {
        json!({
            "issues": { "nodes": [
                { "id": "issue-1", "identifier": "ENG-1", "title": "Sample issue", "description": "A sample issue", "state": { "name": "Todo" } }
            ]}
        })
    } else {
        json!({})
    };

    tracing::info!(query = %query.chars().take(60).collect::<String>(), "graphql request");
    (StatusCode::OK, Json(json!({ "data": data }))).into_response()
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mock_linear=info".into()),
        )
        .init();

    let app = Router::new().route("/graphql", post(graphql));
    let listener = tokio::net::TcpListener::bind(PORT).await.unwrap();
    tracing::info!("mock-linear listening on {PORT}");
    axum::serve(listener, app).await.unwrap();
}
