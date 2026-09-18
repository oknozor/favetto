//! `mock-gmail` — a tiny mock of the Gmail REST API for offline development.
//!
//! It implements the endpoints `favetto-integrations::gmail` uses (labels,
//! messages, threads, send, modify) with deterministic sample data, ignoring the
//! bearer token. Run directly with `cargo run -p mock-gmail`, or via
//! `docker compose up`.

use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use serde_json::{json, Value};

const PORT: &str = "0.0.0.0:4001";

fn b64url(s: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s.as_bytes())
}

fn sample_message(id: &str, subject: &str, body: &str) -> Value {
    json!({
        "id": id,
        "threadId": "thread-1",
        "snippet": body.chars().take(60).collect::<String>(),
        "labelIds": ["INBOX"],
        "payload": {
            "mimeType": "text/plain",
            "headers": [
                { "name": "Subject", "value": subject },
                { "name": "From", "value": "alice@example.com" },
                { "name": "To", "value": "me@example.com" }
            ],
            "body": { "data": b64url(body), "size": body.len() }
        }
    })
}

async fn labels() -> Response {
    Json(json!({
        "labels": [
            { "id": "INBOX", "name": "INBOX", "type": "system" },
            { "id": "UNREAD", "name": "UNREAD", "type": "system" },
            { "id": "Label_1", "name": "Follow-up", "type": "user" }
        ]
    }))
    .into_response()
}

async fn list_messages() -> Response {
    Json(json!({
        "messages": [
            { "id": "msg-1", "threadId": "thread-1" },
            { "id": "msg-2", "threadId": "thread-1" }
        ],
        "resultSizeEstimate": 2
    }))
    .into_response()
}

async fn get_message(Path(id): Path<String>) -> Response {
    match id.as_str() {
        "msg-1" => Json(sample_message("msg-1", "Weekly status", "Hi team, here is this week's status. The favetto project is progressing well and M4 is almost done.")).into_response(),
        "msg-2" => Json(sample_message("msg-2", "Re: Weekly status", "Thanks for the update, keep it up!")).into_response(),
        _ => (StatusCode::NOT_FOUND, Json(json!({ "error": { "message": "not found" } }))).into_response(),
    }
}

async fn get_thread(Path(_id): Path<String>) -> Response {
    Json(json!({
        "id": "thread-1",
        "messages": [
            sample_message("msg-1", "Weekly status", "Hi team, here is this week's status."),
            sample_message("msg-2", "Re: Weekly status", "Thanks for the update, keep it up!")
        ]
    }))
    .into_response()
}

async fn send_message(Json(body): Json<Value>) -> Response {
    let _ = body; // ignore the raw MIME; just acknowledge
    Json(json!({ "id": "sent-1", "threadId": "thread-1" })).into_response()
}

async fn modify_labels(Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let add = body
        .get("addLabelIds")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut msg = sample_message(&id, "Weekly status", "updated");
    if let Some(obj) = msg.as_object_mut() {
        obj.insert("labelIds".to_string(), Value::Array(add));
    }
    Json(msg).into_response()
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mock_gmail=info".into()),
        )
        .init();

    let app = Router::new()
        .route("/gmail/v1/users/me/labels", get(labels))
        .route("/gmail/v1/users/me/messages", get(list_messages))
        .route("/gmail/v1/users/me/messages/send", post(send_message))
        .route("/gmail/v1/users/me/messages/{id}", get(get_message))
        .route("/gmail/v1/users/me/messages/{id}/modify", post(modify_labels))
        .route("/gmail/v1/users/me/threads/{id}", get(get_thread));

    let listener = tokio::net::TcpListener::bind(PORT).await.unwrap();
    tracing::info!("mock-gmail listening on {PORT}");
    axum::serve(listener, app).await.unwrap();
}
