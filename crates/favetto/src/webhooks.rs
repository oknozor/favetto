//! Webhook receivers for GitHub and Linear.
//!
//! Both endpoints verify the provider's HMAC-SHA256 signature before emitting an
//! event on the bus (which also persists it). Signature verification happens on the
//! raw body bytes, so the handlers read `Bytes` and pass it straight to the verifier.
//!
//! - GitHub: `X-Hub-Signature-256: sha256=<hex>` keyed by `GITHUB_WEBHOOK_SECRET`.
//! - Linear: `Linear-Signature: <hex>` keyed by `LINEAR_WEBHOOK_SECRET` (plus
//!   `Linear-Delivery` for idempotency in a later milestone).

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State as AxumState;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::Value;
use sha2::Sha256;
use subtle::ConstantTimeEq;

use favetto_core::model::EventKind;

use crate::state::State;

type HmacSha256 = Hmac<Sha256>;

/// Webhook secrets, read from the environment at daemon startup.
#[derive(Default, Clone)]
pub struct WebhookSecrets {
    pub github: Option<String>,
    pub linear: Option<String>,
}

impl WebhookSecrets {
    pub fn from_env() -> Self {
        Self {
            github: std::env::var("GITHUB_WEBHOOK_SECRET").ok(),
            linear: std::env::var("LINEAR_WEBHOOK_SECRET").ok(),
        }
    }
}

/// Webhook sub-routes. Merged into the daemon's axum app; handlers pull shared state
/// via `State<Arc<State>>`.
pub fn routes() -> Router<Arc<State>> {
    Router::new()
        .route("/webhooks/github", post(github_webhook))
        .route("/webhooks/linear", post(linear_webhook))
}

async fn github_webhook(
    AxumState(state): AxumState<Arc<State>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(secret) = state.webhooks.github.as_deref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "GITHUB_WEBHOOK_SECRET not configured").into_response();
    };

    let sig = match headers.get("x-hub-signature-256").and_then(|v| v.to_str().ok()) {
        Some(s) => s,
        None => return (StatusCode::UNAUTHORIZED, "missing signature").into_response(),
    };

    if !verify_signature(secret, &body, sig.strip_prefix("sha256=").unwrap_or(sig)) {
        return (StatusCode::UNAUTHORIZED, "invalid signature").into_response();
    }

    let event = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid JSON").into_response(),
    };

    let action = payload.get("action").and_then(|a| a.as_str()).unwrap_or("");
    let kind = match (event, action, payload.get("pull_request").is_some()) {
        ("pull_request", "opened", _) => Some(EventKind::PrCreated),
        ("pull_request", "closed", true) => {
            payload.get("pull_request").and_then(|p| p.get("merged")).and_then(|m| m.as_bool())
                .filter(|m| *m)
                .map(|_| EventKind::PrMerged)
        }
        ("issues", "opened", _) => Some(EventKind::IssueCreated),
        ("issues", _, _) => Some(EventKind::IssueUpdated),
        _ => None,
    };

    if let Some(kind) = kind {
        state.emit_event(kind, summarize_github(payload)).await;
    }

    StatusCode::OK.into_response()
}

async fn linear_webhook(
    AxumState(state): AxumState<Arc<State>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(secret) = state.webhooks.linear.as_deref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "LINEAR_WEBHOOK_SECRET not configured").into_response();
    };

    let sig = match headers.get("linear-signature").and_then(|v| v.to_str().ok()) {
        Some(s) => s,
        None => return (StatusCode::UNAUTHORIZED, "missing signature").into_response(),
    };

    if !verify_signature(secret, &body, sig) {
        return (StatusCode::UNAUTHORIZED, "invalid signature").into_response();
    }

    let payload: Value = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid JSON").into_response(),
    };

    let action = payload.get("action").and_then(|a| a.as_str()).unwrap_or("");
    let kind = match action {
        "create" => Some(EventKind::TicketCreated),
        "update" => Some(EventKind::TicketUpdated),
        _ => None,
    };

    if let Some(kind) = kind {
        state.emit_event(kind, payload).await;
    }

    StatusCode::OK.into_response()
}

/// Reduce a GitHub payload to the fields the event log cares about.
fn summarize_github(payload: Value) -> Value {
    let repo = payload
        .get("repository")
        .and_then(|r| r.get("full_name"))
        .and_then(|n| n.as_str())
        .unwrap_or("");
    let number = payload
        .get("issue")
        .or_else(|| payload.get("pull_request"))
        .and_then(|i| i.get("number"))
        .cloned()
        .unwrap_or(Value::Null);
    let title = payload
        .get("issue")
        .or_else(|| payload.get("pull_request"))
        .and_then(|i| i.get("title"))
        .and_then(|t| t.as_str())
        .unwrap_or("");
    serde_json::json!({
        "action": payload.get("action"),
        "repo": repo,
        "number": number,
        "title": title,
    })
}

/// Compute HMAC-SHA256 over `body` and compare against `signature` (hex) in constant time.
fn verify_signature(secret: &str, body: &[u8], signature: &str) -> bool {
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    let digest = mac.finalize().into_bytes();
    let expected = hex::encode(digest);
    expected.as_bytes().ct_eq(signature.as_bytes()).into()
}
