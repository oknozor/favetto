//! Username/password login (`POST /auth/login`).
//!
//! A remote client that cannot run `favetto pair` on the daemon host (a phone,
//! a browser) can exchange a username and password for the same bearer
//! [`Token`](favetto_core::auth::Token) the pairing flow returns. Passwords are
//! verified with argon2id in constant time ([`favetto_core::auth`]) and failed
//! attempts are throttled per `(username, client)`.
//!
//! The route is guarded by the `require_tls` setting: when enabled, a
//! non-loopback login must arrive over HTTPS (directly or via a proxy that sets
//! `X-Forwarded-Proto: https`). Passwords are never logged.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::connect_info::ConnectInfo;
use axum::extract::State as AxumState;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;

use favetto_core::auth::verify_password;

use crate::db;
use crate::state::State;

/// In-memory failed-login counter, keyed by `(username, client)`.
///
/// State is per-process and reset on restart, which is enough to blunt online
/// password guessing without a durable store. An entry records the number of
/// failures and the instant of the first one; entries older than `window` are
/// pruned on every call.
pub struct LoginLimiter {
    window: Duration,
    max: u32,
    failures: Mutex<HashMap<String, (u32, Instant)>>,
}

impl LoginLimiter {
    /// A limiter that locks a key after `max` failures within `window_secs`.
    /// `max == 0` disables the limit.
    pub fn new(max: u32, window_secs: u64) -> Self {
        Self {
            window: Duration::from_secs(window_secs),
            max,
            failures: Mutex::new(HashMap::new()),
        }
    }

    /// Whether `key` has reached the failure limit inside the current window.
    pub async fn is_locked(&self, key: &str) -> bool {
        let mut failures = self.failures.lock().await;
        self.prune(&mut failures, Instant::now());
        if self.max == 0 {
            return false;
        }
        failures
            .get(key)
            .map(|(count, _)| *count >= self.max)
            .unwrap_or(false)
    }

    /// Record one failed attempt for `key`.
    pub async fn record_failure(&self, key: &str) {
        let mut failures = self.failures.lock().await;
        let now = Instant::now();
        self.prune(&mut failures, now);
        let entry = failures.entry(key.to_string()).or_insert((0, now));
        entry.0 = entry.0.saturating_add(1);
    }

    /// Forget the recorded failures for `key` (a successful login).
    pub async fn clear(&self, key: &str) {
        self.failures.lock().await.remove(key);
    }

    /// Drop entries whose window has elapsed.
    fn prune(&self, failures: &mut HashMap<String, (u32, Instant)>, now: Instant) {
        failures.retain(|_, (_, first)| now.duration_since(*first) < self.window);
    }
}

/// Login sub-route (`/auth/login`).
pub(crate) fn routes() -> Router<Arc<State>> {
    Router::new().route("/auth/login", post(login))
}

#[derive(Deserialize)]
struct LoginBody {
    username: String,
    password: String,
}

/// `POST /auth/login` — exchange a username/password for the bearer token.
async fn login(
    AxumState(state): AxumState<Arc<State>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<LoginBody>,
) -> Response {
    if state.config.auth.require_tls && !tls_allowed(&headers, addr) {
        return (
            StatusCode::BAD_REQUEST,
            "login requires TLS: connect over https:// (or set X-Forwarded-Proto)",
        )
            .into_response();
    }

    let key = format!("{}:{}", body.username, client_key(&headers, addr));
    if state.login_limiter.is_locked(&key).await {
        tracing::warn!(username = %body.username, "login rejected: rate limited");
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "too many failed login attempts; try again later",
        )
            .into_response();
    }

    let hash = match db::get_user_hash(&state.db, &body.username).await {
        Ok(hash) => hash,
        Err(e) => {
            tracing::error!(error = %e, "login failed: user lookup error");
            return (StatusCode::INTERNAL_SERVER_ERROR, "login failed").into_response();
        }
    };

    let Some(hash) = hash else {
        // Distinguish "no users configured at all" from "unknown user" so the
        // first remote login surfaces the setup command instead of a lockout.
        let users = db::count_users(&state.db).await.unwrap_or(0);
        if users == 0 {
            tracing::warn!(username = %body.username, "login rejected: no users configured");
            return (
                StatusCode::UNAUTHORIZED,
                "no users configured; run `favetto user add <name>` on the daemon host",
            )
                .into_response();
        }
        state.login_limiter.record_failure(&key).await;
        tracing::warn!(username = %body.username, "login failed: unknown user");
        return (StatusCode::UNAUTHORIZED, "invalid username or password").into_response();
    };

    if !verify_password(&body.password, &hash) {
        state.login_limiter.record_failure(&key).await;
        tracing::warn!(username = %body.username, "login failed: wrong password");
        return (StatusCode::UNAUTHORIZED, "invalid username or password").into_response();
    }

    state.login_limiter.clear(&key).await;
    tracing::info!(username = %body.username, "login succeeded");
    Json(json!({ "token": state.token.as_str() })).into_response()
}

/// True when `addr` is loopback, or the request was forwarded over HTTPS.
fn tls_allowed(headers: &HeaderMap, addr: SocketAddr) -> bool {
    addr.ip().is_loopback() || forwarded_proto_is_https(headers)
}

/// True when a proxy reports the original request used HTTPS.
fn forwarded_proto_is_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .map(|proto| proto.eq_ignore_ascii_case("https"))
        .unwrap_or(false)
}

/// The rate-limit bucket for a client: the peer IP, or (when the peer is a
/// loopback reverse proxy) the first forwarded hop.
fn client_key(headers: &HeaderMap, addr: SocketAddr) -> String {
    if addr.ip().is_loopback() {
        if let Some(forwarded) = forwarded_client(headers) {
            return forwarded;
        }
    }
    addr.ip().to_string()
}

/// The first address in `X-Forwarded-For`, else `X-Real-IP`.
fn forwarded_client(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = value.split(',').next() {
            let first = first.trim();
            if !first.is_empty() {
                return Some(first.to_string());
            }
        }
    }
    headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::{to_bytes, Body};
    use axum::extract::connect_info::MockConnectInfo;
    use axum::http::Request;
    use tower::ServiceExt as _;

    use crate::config::FavettoConfig;
    use crate::state::StateInit;

    /// A `State` backed by a scratch SQLite database and the given config.
    async fn test_state(config: FavettoConfig) -> Arc<State> {
        let dir = std::env::temp_dir().join(format!("favetto-auth-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::db::open(&dir.join("test.db")).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        Arc::new(State::new(StateInit {
            db: pool,
            bus: crate::event_bus::EventBus::new(8),
            token: favetto_core::auth::Token::generate(),
            webhooks: crate::webhooks::WebhookSecrets::from_config(&config),
            agents: crate::agents::AgentManager::new(),
            registry: crate::agents::AgentRegistry::default(),
            config: Arc::new(config),
            data_dir: dir.clone(),
            tasks_dir: dir,
            catalog: Arc::new(parking_lot::RwLock::new(Vec::new())),
            scheduler: tokio_cron_scheduler::JobScheduler::new().await.unwrap(),
            hook_store: Arc::new(parking_lot::RwLock::new(Vec::new())),
        }))
    }

    fn addr(ip: [u8; 4]) -> SocketAddr {
        SocketAddr::from((ip, 4321))
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        headers
    }

    #[tokio::test]
    async fn limiter_locks_after_max_failures_and_expires() {
        let limiter = LoginLimiter {
            window: Duration::from_millis(50),
            max: 3,
            failures: Mutex::new(HashMap::new()),
        };
        let key = "alice:127.0.0.1";

        assert!(!limiter.is_locked(key).await);
        for _ in 0..2 {
            limiter.record_failure(key).await;
        }
        assert!(!limiter.is_locked(key).await, "below the limit");
        limiter.record_failure(key).await;
        assert!(limiter.is_locked(key).await, "at the limit");

        // A success clears the counter.
        limiter.clear(key).await;
        assert!(!limiter.is_locked(key).await);

        // Failures age out of the window.
        for _ in 0..3 {
            limiter.record_failure(key).await;
        }
        assert!(limiter.is_locked(key).await);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(!limiter.is_locked(key).await, "window elapsed");
    }

    #[tokio::test]
    async fn limiter_max_zero_disables_the_limit() {
        let limiter = LoginLimiter::new(0, 300);
        for _ in 0..100 {
            limiter.record_failure("k").await;
        }
        assert!(!limiter.is_locked("k").await);
    }

    #[test]
    fn client_key_prefers_the_peer_unless_it_is_a_loopback_proxy() {
        // A direct remote client is keyed by its own address.
        assert_eq!(
            client_key(
                &headers(&[("x-forwarded-for", "203.0.113.7")]),
                addr([192, 168, 1, 5])
            ),
            "192.168.1.5"
        );
        // A loopback peer is a proxy: trust the forwarded hop.
        assert_eq!(
            client_key(
                &headers(&[("x-forwarded-for", "203.0.113.7, 10.0.0.1")]),
                addr([127, 0, 0, 1])
            ),
            "203.0.113.7"
        );
        assert_eq!(
            client_key(
                &headers(&[("x-real-ip", "203.0.113.9")]),
                addr([127, 0, 0, 1])
            ),
            "203.0.113.9"
        );
        // No forwarded header: fall back to the peer.
        assert_eq!(
            client_key(&HeaderMap::new(), addr([127, 0, 0, 1])),
            "127.0.0.1"
        );
    }

    #[test]
    fn tls_allowed_only_for_loopback_or_forwarded_https() {
        assert!(tls_allowed(&HeaderMap::new(), addr([127, 0, 0, 1])));
        assert!(!tls_allowed(&HeaderMap::new(), addr([192, 168, 1, 5])));
        assert!(tls_allowed(
            &headers(&[("x-forwarded-proto", "https")]),
            addr([192, 168, 1, 5])
        ));
        // A non-https or malformed proto does not pass.
        assert!(!tls_allowed(
            &headers(&[("x-forwarded-proto", "http")]),
            addr([192, 168, 1, 5])
        ));
    }

    /// Drive the real router with a mocked peer address.
    async fn post_login(
        state: Arc<State>,
        addr: SocketAddr,
        body: serde_json::Value,
    ) -> (StatusCode, Vec<u8>) {
        let app = routes().with_state(state).layer(MockConnectInfo(addr));
        let request = Request::builder()
            .method("POST")
            .uri("/auth/login")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, bytes.to_vec())
    }

    #[tokio::test]
    async fn login_returns_the_token_for_a_correct_password() {
        let state = test_state(FavettoConfig::default()).await;
        let hash = favetto_core::auth::hash_password("s3cret").unwrap();
        crate::db::upsert_user(&state.db, "alice", &hash)
            .await
            .unwrap();

        let (status, bytes) = post_login(
            state.clone(),
            addr([127, 0, 0, 1]),
            json!({ "username": "alice", "password": "s3cret" }),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&bytes)
        );
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["token"], json!(state.token.as_str()));
    }

    #[tokio::test]
    async fn login_rejects_a_wrong_password() {
        let state = test_state(FavettoConfig::default()).await;
        let hash = favetto_core::auth::hash_password("s3cret").unwrap();
        crate::db::upsert_user(&state.db, "alice", &hash)
            .await
            .unwrap();

        let (status, _) = post_login(
            state.clone(),
            addr([127, 0, 0, 1]),
            json!({ "username": "alice", "password": "nope" }),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        // Wrong credentials record a failure for the key.
        let key = "alice:127.0.0.1";
        assert!(
            !state.login_limiter.is_locked(key).await,
            "one failure is below the default limit"
        );
        assert_eq!(
            state
                .login_limiter
                .failures
                .lock()
                .await
                .get(key)
                .map(|(count, _)| *count),
            Some(1)
        );
    }

    #[tokio::test]
    async fn login_with_no_users_returns_a_setup_hint() {
        let state = test_state(FavettoConfig::default()).await;
        let (status, bytes) = post_login(
            state,
            addr([127, 0, 0, 1]),
            json!({ "username": "alice", "password": "whatever" }),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let message = String::from_utf8_lossy(&bytes);
        assert!(
            message.contains("favetto user add"),
            "expected setup hint, got {message}"
        );
    }

    #[tokio::test]
    async fn login_with_an_unknown_user_is_rejected() {
        let state = test_state(FavettoConfig::default()).await;
        crate::db::upsert_user(&state.db, "alice", "hash")
            .await
            .unwrap();
        let (status, bytes) = post_login(
            state,
            addr([127, 0, 0, 1]),
            json!({ "username": "mallory", "password": "x" }),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(!String::from_utf8_lossy(&bytes).contains("favetto user add"));
    }

    #[tokio::test]
    async fn login_is_rate_limited_after_repeated_failures() {
        let config = FavettoConfig {
            auth: crate::config::AuthSettings {
                login_rate_limit: 2,
                ..Default::default()
            },
            ..Default::default()
        };
        let state = test_state(config).await;
        let hash = favetto_core::auth::hash_password("s3cret").unwrap();
        crate::db::upsert_user(&state.db, "alice", &hash)
            .await
            .unwrap();

        for _ in 0..2 {
            let (status, _) = post_login(
                state.clone(),
                addr([127, 0, 0, 1]),
                json!({ "username": "alice", "password": "nope" }),
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }
        // The third attempt is refused before verification, even with a good
        // password.
        let (status, _) = post_login(
            state,
            addr([127, 0, 0, 1]),
            json!({ "username": "alice", "password": "s3cret" }),
        )
        .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn login_requires_tls_for_non_loopback_when_configured() {
        let config = FavettoConfig {
            auth: crate::config::AuthSettings {
                require_tls: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let state = test_state(config).await;
        let hash = favetto_core::auth::hash_password("s3cret").unwrap();
        crate::db::upsert_user(&state.db, "alice", &hash)
            .await
            .unwrap();

        // A non-loopback plaintext request is rejected before verification.
        let (status, _) = post_login(
            state.clone(),
            addr([192, 168, 1, 5]),
            json!({ "username": "alice", "password": "s3cret" }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // Loopback is trusted.
        let (status, _) = post_login(
            state.clone(),
            addr([127, 0, 0, 1]),
            json!({ "username": "alice", "password": "s3cret" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // A forwarded HTTPS request from the same non-loopback client passes.
        let app = routes()
            .with_state(state)
            .layer(MockConnectInfo(addr([192, 168, 1, 5])));
        let request = Request::builder()
            .method("POST")
            .uri("/auth/login")
            .header("content-type", "application/json")
            .header("x-forwarded-proto", "https")
            .body(Body::from(
                serde_json::to_vec(&json!({ "username": "alice", "password": "s3cret" })).unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
