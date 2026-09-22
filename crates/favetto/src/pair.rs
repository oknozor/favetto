//! Pairing flow: a short-lived code exchanged for the daemon's bearer token, so a
//! remote TUI can be attached without copying the token by hand.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State as AxumState;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::state::State;

/// How long a pairing code stays valid.
pub const PAIR_TTL_SECS: u64 = 60;

/// In-memory store of active pairing codes.
pub struct PairStore {
    codes: Mutex<HashMap<String, Instant>>,
}

impl PairStore {
    pub fn new() -> Self {
        Self {
            codes: Mutex::new(HashMap::new()),
        }
    }

    pub async fn issue(&self, code: String) {
        self.codes.lock().await.insert(code, Instant::now());
    }

    /// Validate + consume a code, returning true if it was valid and unexpired.
    pub async fn redeem(&self, code: &str) -> bool {
        let mut codes = self.codes.lock().await;
        match codes.remove(code) {
            Some(issued) => issued.elapsed() < Duration::from_secs(PAIR_TTL_SECS),
            None => false,
        }
    }
}

/// Pairing sub-routes (`/pair/generate`, `/pair/exchange`).
pub fn routes() -> Router<Arc<State>> {
    Router::new()
        .route("/pair/generate", post(generate))
        .route("/pair/exchange", post(exchange))
}

fn random_code() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    format!("{:06}", (nanos % 1_000_000))
}

async fn generate(AxumState(state): AxumState<Arc<State>>) -> Response {
    let code = random_code();
    state.pair.issue(code.clone()).await;
    tracing::info!(code = %code, "issued pairing code");
    (
        StatusCode::OK,
        Json(json!({ "code": code, "expires_in": PAIR_TTL_SECS })),
    )
        .into_response()
}

#[derive(Deserialize)]
struct ExchangeBody {
    code: String,
}

async fn exchange(
    AxumState(state): AxumState<Arc<State>>,
    Json(body): Json<ExchangeBody>,
) -> Response {
    if state.pair.redeem(&body.code).await {
        Json(json!({ "token": state.token.as_str() })).into_response()
    } else {
        (StatusCode::UNAUTHORIZED, "invalid or expired pairing code").into_response()
    }
}

/// `favetto pair`: request a code from the daemon and print it for the user.
pub async fn run(args: crate::cli::PairArgs) -> anyhow::Result<()> {
    let resp: Value = reqwest::Client::new()
        .post(format!("{}/pair/generate", args.url.trim_end_matches('/')))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let code = resp
        .get("code")
        .and_then(|c| c.as_str())
        .ok_or_else(|| anyhow::anyhow!("pair/generate response missing code"))?;
    let expires = resp
        .get("expires_in")
        .and_then(|e| e.as_u64())
        .unwrap_or(crate::pair::PAIR_TTL_SECS);

    println!("pairing code (valid {expires}s): {code}");
    println!("attach with: favetto tui --remote ws://HOST:7878 --pair-code {code}");
    Ok(())
}
