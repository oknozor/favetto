//! Short-lived, single-use tickets that authenticate a header-less WebSocket.
//!
//! A browser `WebSocket` cannot set an `Authorization` header, so a client first
//! exchanges its bearer token for a ticket at `POST /auth/ticket` and then passes
//! it as `?ticket=` on the upgrade. Tickets are random, in-memory, single-use,
//! and validated through the same constant-time comparison as the bearer token,
//! following the shape of [`crate::pair::PairStore`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State as AxumState;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::json;
use tokio::sync::Mutex;

use favetto_core::auth::Token;

use crate::state::State;

/// In-memory store of active, single-use tickets.
///
/// Shaped like [`crate::pair::PairStore`]: `new`/`issue`/`redeem` behind an async
/// mutex. Unlike a pairing code, a ticket is verified with a constant-time
/// comparison (the bearer token's own [`Token::verify`] path) and is consumed by
/// the first valid use, so a replayed ticket is always rejected.
pub struct TicketStore {
    ttl: Duration,
    tickets: Mutex<Vec<(Token, Instant)>>,
}

impl TicketStore {
    /// Create an empty store whose tickets live for `ttl_secs` seconds.
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            ttl: Duration::from_secs(ttl_secs),
            tickets: Mutex::new(Vec::new()),
        }
    }

    /// The configured ticket lifetime, in whole seconds.
    pub fn ttl_secs(&self) -> u64 {
        self.ttl.as_secs()
    }

    /// Mint a fresh random ticket and remember it until it is redeemed or times out.
    pub async fn issue(&self) -> String {
        let token = Token::generate();
        let ticket = token.as_str().to_string();
        let mut tickets = self.tickets.lock().await;
        let now = Instant::now();
        // Drop already-expired tickets so a stream of issued-but-unused tickets
        // cannot grow the in-memory store without bound.
        tickets.retain(|(_, issued)| now.duration_since(*issued) < self.ttl);
        tickets.push((token, now));
        ticket
    }

    /// Validate + consume a ticket, returning true if it was valid and unexpired.
    ///
    /// Every stored ticket is compared through [`Token::verify`] (the same
    /// constant-time path the bearer token uses); the scan never short-circuits,
    /// so a miss costs the same as a hit. A matched ticket is always removed,
    /// which makes it single-use even when it has already expired.
    pub async fn redeem(&self, presented: &str) -> bool {
        let mut tickets = self.tickets.lock().await;
        let now = Instant::now();
        let mut matched = None;
        for (i, (token, _)) in tickets.iter().enumerate() {
            if token.verify(presented) && matched.is_none() {
                matched = Some(i);
            }
        }
        match matched {
            Some(i) => {
                let (_, issued) = tickets.remove(i);
                now.duration_since(issued) < self.ttl
            }
            None => false,
        }
    }
}

/// Ticket sub-route (`/auth/ticket`).
pub(crate) fn routes() -> Router<Arc<State>> {
    Router::new().route("/auth/ticket", post(issue_ticket))
}

/// Whether the request carries a valid `Authorization: Bearer <token>` header.
///
/// The same check guards the WebSocket upgrade and the ticket endpoint.
pub(crate) fn bearer_authorized(token: &Token, headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| token.verify(t))
        .unwrap_or(false)
}

/// `POST /auth/ticket` — exchange a bearer token for a short-lived WS ticket.
async fn issue_ticket(AxumState(state): AxumState<Arc<State>>, headers: HeaderMap) -> Response {
    if !bearer_authorized(&state.token, &headers) {
        return (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response();
    }

    let ticket = state.tickets.issue().await;
    Json(json!({
        "ticket": ticket,
        "expires_in": state.tickets.ttl_secs(),
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
    use axum::response::IntoResponse;

    use crate::state::StateInit;

    /// A `State` backed by a scratch SQLite database, for exercising the handler.
    async fn test_state() -> Arc<State> {
        let dir = std::env::temp_dir().join(format!("favetto-ticket-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::db::open(&dir.join("test.db")).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        Arc::new(State::new(StateInit {
            db: pool,
            bus: crate::event_bus::EventBus::new(8),
            token: Token::generate(),
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

    fn bearer_headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
        headers
    }

    #[tokio::test]
    async fn ticket_is_single_use() {
        let store = TicketStore::new(30);
        let ticket = store.issue().await;

        assert!(store.redeem(&ticket).await, "first use succeeds");
        assert!(!store.redeem(&ticket).await, "replay is rejected");
        assert!(!store.redeem("not-a-ticket").await, "unknown is rejected");
    }

    #[tokio::test]
    async fn expired_ticket_is_rejected() {
        // A zero TTL means every ticket is already expired when redeemed.
        let store = TicketStore::new(0);
        let ticket = store.issue().await;
        assert!(!store.redeem(&ticket).await);
    }

    #[tokio::test]
    async fn tickets_are_independent() {
        let store = TicketStore::new(30);
        let a = store.issue().await;
        let b = store.issue().await;

        assert_ne!(a, b);
        assert!(store.redeem(&b).await);
        // Consuming one ticket leaves the other usable exactly once.
        assert!(store.redeem(&a).await);
        assert!(!store.redeem(&a).await);
    }

    #[test]
    fn ttl_is_configurable() {
        assert_eq!(TicketStore::new(45).ttl_secs(), 45);
        // The built-in default lives in the config (`[auth].ticket_ttl_secs`).
        assert_eq!(
            crate::config::FavettoConfig::default().auth.ticket_ttl_secs,
            30
        );
    }

    #[tokio::test]
    async fn endpoint_issues_a_ticket_for_a_valid_bearer() {
        let state = test_state().await;
        let headers = bearer_headers(state.token.as_str());

        let resp = issue_ticket(AxumState(state.clone()), headers)
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );

        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let ticket = body["ticket"].as_str().unwrap();
        assert_eq!(body["expires_in"], json!(state.tickets.ttl_secs()));
        // The returned ticket is redeemable exactly once.
        assert!(state.tickets.redeem(ticket).await);
        assert!(!state.tickets.redeem(ticket).await);
    }

    #[tokio::test]
    async fn endpoint_rejects_a_missing_or_wrong_bearer() {
        let state = test_state().await;

        let missing = issue_ticket(AxumState(state.clone()), HeaderMap::new())
            .await
            .into_response();
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);

        let wrong = issue_ticket(AxumState(state), bearer_headers("nope"))
            .await
            .into_response();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    }
}
