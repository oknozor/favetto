//! Pairing flow: a short-lived code exchanged for the daemon's bearer token, so a
//! remote TUI can be attached without copying the token by hand.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State as AxumState;
use axum::http::{HeaderMap, StatusCode};
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
pub(crate) struct PairStore {
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
pub(crate) fn routes() -> Router<Arc<State>> {
    Router::new()
        .route("/pair/generate", post(generate))
        .route("/pair/exchange", post(exchange))
}

/// A six-digit code derived from a UUID v4, so it is unpredictable rather than
/// clock-derived.
fn random_code() -> String {
    let uuid = uuid::Uuid::new_v4();
    let bytes = uuid.as_bytes();
    let n = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    format!("{:06}", n % 1_000_000)
}

/// `POST /pair/generate` — mint a pairing code. Requires a valid bearer token so
/// the endpoint can no longer dispense the token to an unauthenticated caller.
async fn generate(AxumState(state): AxumState<Arc<State>>, headers: HeaderMap) -> Response {
    if !crate::server::bearer_authorized(&headers, &state.token) {
        return (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response();
    }
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
///
/// Reads the bearer token from `<data_dir>/token` (or `--token-file`) and sends
/// it, because `POST /pair/generate` is bearer-guarded.
///
/// The two `println!` lines at the end are this command's stdout contract, not
/// diagnostics: scripts parse the pairing code from the first line and the
/// ready-to-run attach command from the second. Keep them on stdout; route any
/// new status or error detail through `tracing` instead.
pub async fn run(args: crate::cli::PairArgs) -> anyhow::Result<()> {
    let token_path = args
        .token_file
        .clone()
        .unwrap_or_else(crate::cli::default_token_path);
    let token = std::fs::read_to_string(&token_path).map_err(|e| {
        anyhow::anyhow!(
            "failed to read bearer token at {}: {e}. Start the daemon (or run `favetto token-rotate`) to create it, or pass --token-file.",
            token_path.display()
        )
    })?;

    let resp: Value = reqwest::Client::new()
        .post(format!("{}/pair/generate", args.url.trim_end_matches('/')))
        .bearer_auth(token.trim())
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
    println!("attach with: favetto tui --remote ws://HOST:7878/rpc --pair-code {code}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::to_bytes;
    use axum::http::header::AUTHORIZATION;

    use crate::state::StateInit;

    async fn test_state() -> Arc<State> {
        let dir = std::env::temp_dir().join(format!("favetto-pair-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::db::open(&dir.join("test.db")).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        Arc::new(State::new(StateInit {
            config_path: std::path::PathBuf::new(),
            db: pool,
            bus: crate::event_bus::EventBus::new(8),
            token: favetto_core::auth::Token::generate(),
            webhooks: crate::webhooks::WebhookSecrets::from_config(
                &crate::config::FavettoConfig::default(),
            ),
            agents: crate::agents::AgentManager::new(),
            registry: crate::agents::AgentRegistry::default(),
            config: Arc::new(crate::config::FavettoConfig::default()),
            data_dir: dir.clone(),
            tasks_dir: dir,
            catalog: Arc::new(parking_lot::RwLock::new(Vec::new())),
            scheduler: tokio_cron_scheduler::JobScheduler::new().await.unwrap(),
            hook_store: Arc::new(parking_lot::RwLock::new(Vec::new())),
        }))
    }

    fn bearer(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
        headers
    }

    #[test]
    fn codes_are_six_digits() {
        for _ in 0..64 {
            let code = random_code();
            assert_eq!(code.len(), 6);
            assert!(code.chars().all(|c| c.is_ascii_digit()), "{code}");
        }
    }

    #[tokio::test]
    async fn generate_requires_a_bearer_token() {
        let state = test_state().await;

        let response = generate(AxumState(state.clone()), HeaderMap::new())
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = generate(AxumState(state.clone()), bearer("wrong"))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        // No code was issued.
        assert!(state.pair.codes.lock().await.is_empty());
    }

    #[tokio::test]
    async fn generate_issues_a_redeemable_code_with_a_bearer() {
        let state = test_state().await;
        let response = generate(AxumState(state.clone()), bearer(state.token.as_str()))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);

        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let code = body["code"].as_str().unwrap();
        assert_eq!(body["expires_in"], json!(PAIR_TTL_SECS));
        assert!(state.pair.redeem(code).await);
        assert!(!state.pair.redeem(code).await, "single use");
    }

    #[tokio::test]
    async fn exchange_redeems_a_code_for_the_token() {
        let state = test_state().await;
        state.pair.issue("123456".to_string()).await;

        let response = exchange(
            AxumState(state.clone()),
            Json(ExchangeBody {
                code: "123456".to_string(),
            }),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["token"], json!(state.token.as_str()));

        // A consumed code is rejected.
        let response = exchange(
            AxumState(state),
            Json(ExchangeBody {
                code: "123456".to_string(),
            }),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
