use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::Router;
use tower::ServiceExt as _;

use crate::config::FavettoConfig;
use crate::state::{State, StateInit};

/// The marker in the committed `web/dist/index.html` placeholder.
const EMBEDDED_MARKER: &str = "favetto-placeholder";
/// The marker the disk-index fixture writes, distinct from the embedded one.
const DISK_MARKER: &str = "disk-index";

/// A unique scratch directory for web tests.
fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("favetto-web-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A `State` backed by a scratch SQLite database and the given web config.
async fn web_state(config: FavettoConfig) -> Arc<State> {
    let dir = temp_dir("state");
    let tasks_dir = dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let pool = crate::db::open(&dir.join("test.db")).await.unwrap();
    crate::db::migrate(&pool).await.unwrap();
    let catalog = Arc::new(parking_lot::RwLock::new(
        crate::tasks::load_catalog(&tasks_dir).unwrap(),
    ));
    let scheduler = tokio_cron_scheduler::JobScheduler::new().await.unwrap();
    Arc::new(State::new(StateInit {
        db: pool,
        bus: crate::event_bus::EventBus::new(64),
        token: favetto_core::auth::Token::generate(),
        webhooks: crate::webhooks::WebhookSecrets::from_config(&config),
        agents: crate::agents::AgentManager::new(),
        registry: crate::agents::AgentRegistry::default(),
        config: Arc::new(config),
        data_dir: dir.clone(),
        tasks_dir,
        catalog,
        scheduler,
        hook_store: Arc::new(parking_lot::RwLock::new(Vec::new())),
    }))
}

/// Create a disk asset directory with stable content and the `disk-index` marker.
fn disk_assets(tag: &str) -> PathBuf {
    let dir = temp_dir(tag);
    std::fs::create_dir_all(dir.join("assets")).unwrap();
    std::fs::write(
        dir.join("index.html"),
        format!("<!doctype html><html><body id=\"{DISK_MARKER}\"></body></html>"),
    )
    .unwrap();
    std::fs::write(dir.join("assets/app.js"), b"console.log('app')").unwrap();
    std::fs::write(dir.join("assets/app.css"), b"body{}").unwrap();
    std::fs::write(dir.join("assets/app_bg.wasm"), b"\0asm").unwrap();
    dir
}

/// A config that serves from disk at `dir`.
fn disk_config(dir: &Path) -> FavettoConfig {
    FavettoConfig {
        web: crate::config::WebSettings {
            dir: dir.to_path_buf(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Drive `app` with one request and collect status, headers and body.
async fn request(
    app: Router,
    method: Method,
    uri: &str,
    accept: Option<&str>,
    body: Vec<u8>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(accept) = accept {
        builder = builder.header(header::ACCEPT, accept);
    }
    let res = app
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, headers, bytes.to_vec())
}

fn body_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn content_type(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
}

fn cache_control(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
}

/// The web-only router (no API routes), for isolating asset/fallback behaviour.
fn web_router(state: &Arc<State>) -> Router {
    crate::web::routes().with_state(state.clone())
}

#[tokio::test]
async fn index_serves_embedded_html() {
    let state = web_state(FavettoConfig::default()).await;
    let app = web_router(&state);

    let (status, headers, bytes) = request(app, Method::GET, "/", None, Vec::new()).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), Some("text/html; charset=utf-8"));
    assert_eq!(cache_control(&headers), Some("no-cache"));
    assert!(
        body_text(&bytes).contains(EMBEDDED_MARKER),
        "{}",
        body_text(&bytes)
    );
}

#[tokio::test]
async fn assets_serve_explicit_mime_types() {
    let dir = disk_assets("mime");
    let state = web_state(disk_config(&dir)).await;
    let app = web_router(&state);

    let cases = [
        ("/assets/app_bg.wasm", "application/wasm"),
        ("/assets/app.js", "text/javascript"),
        ("/assets/app.css", "text/css"),
    ];
    for (uri, expected) in cases {
        let (status, headers, _) = request(app.clone(), Method::GET, uri, None, Vec::new()).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(content_type(&headers), Some(expected), "{uri}");
    }
}

#[tokio::test]
async fn hashed_assets_are_immutable() {
    let dir = disk_assets("immutable");
    let state = web_state(disk_config(&dir)).await;
    let app = web_router(&state);

    let (status, headers, _) = request(app, Method::GET, "/assets/app.js", None, Vec::new()).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        cache_control(&headers),
        Some("public, max-age=31536000, immutable")
    );
}

#[tokio::test]
async fn unknown_asset_is_404() {
    let dir = disk_assets("unknown");
    let state = web_state(disk_config(&dir)).await;
    let app = web_router(&state);

    let (status, _, _) = request(app, Method::GET, "/assets/nope.js", None, Vec::new()).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn spa_fallback_serves_index_for_html_accept() {
    let dir = disk_assets("fallback-ok");
    let state = web_state(disk_config(&dir)).await;
    let app = crate::daemon::http_app(&state);

    let (status, headers, bytes) = request(
        app,
        Method::GET,
        "/inbox/abc",
        Some("text/html"),
        Vec::new(),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), Some("text/html; charset=utf-8"));
    assert!(
        body_text(&bytes).contains(DISK_MARKER),
        "{}",
        body_text(&bytes)
    );
}

#[tokio::test]
async fn spa_fallback_rejects_non_html() {
    let dir = disk_assets("fallback-non-html");
    let state = web_state(disk_config(&dir)).await;
    let app = crate::daemon::http_app(&state);

    let (no_accept, _, _) = request(app.clone(), Method::GET, "/inbox/abc", None, Vec::new()).await;
    assert_eq!(no_accept, StatusCode::NOT_FOUND);

    let (json, _, _) = request(
        app,
        Method::GET,
        "/inbox/abc",
        Some("application/json"),
        Vec::new(),
    )
    .await;
    assert_eq!(json, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn spa_fallback_rejects_non_get() {
    let state = web_state(FavettoConfig::default()).await;
    let app = crate::daemon::http_app(&state);

    let (status, _, _) = request(
        app,
        Method::POST,
        "/inbox/abc",
        Some("text/html"),
        Vec::new(),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn index_ignores_accept_for_root() {
    let state = web_state(FavettoConfig::default()).await;
    let app = web_router(&state);

    let (status, _, bytes) = request(app, Method::GET, "/", None, Vec::new()).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body_text(&bytes).contains(EMBEDDED_MARKER));
}

#[tokio::test]
async fn api_routes_are_never_shadowed() {
    let state = web_state(FavettoConfig::default()).await;
    let app = crate::daemon::http_app(&state);

    // `GET /events` without a bearer token stays an auth failure, even though
    // the request advertises `Accept: text/html`.
    let (events, _, events_body) = request(
        app.clone(),
        Method::GET,
        "/events",
        Some("text/html"),
        Vec::new(),
    )
    .await;
    assert_eq!(events, StatusCode::UNAUTHORIZED);
    assert!(!body_text(&events_body).contains(EMBEDDED_MARKER));

    // `GET /metrics` renders the metrics registry, not the SPA.
    let (metrics, _, metrics_body) = request(
        app.clone(),
        Method::GET,
        "/metrics",
        Some("text/html"),
        Vec::new(),
    )
    .await;
    assert_eq!(metrics, StatusCode::OK);
    assert!(body_text(&metrics_body).contains("favetto_tasks_total"));
    assert!(!body_text(&metrics_body).contains(EMBEDDED_MARKER));

    // `GET /rpc` without WebSocket upgrade headers is rejected by the extractor.
    let (rpc_get, _, rpc_get_body) = request(
        app.clone(),
        Method::GET,
        "/rpc",
        Some("text/html"),
        Vec::new(),
    )
    .await;
    assert_ne!(rpc_get, StatusCode::OK);
    assert!(!body_text(&rpc_get_body).contains(EMBEDDED_MARKER));

    // `POST /rpc` without a bearer token stays a 401.
    let (rpc_post, _, rpc_post_body) = request(
        app.clone(),
        Method::POST,
        "/rpc",
        Some("text/html"),
        Vec::new(),
    )
    .await;
    assert_eq!(rpc_post, StatusCode::UNAUTHORIZED);
    assert!(!body_text(&rpc_post_body).contains(EMBEDDED_MARKER));

    // Webhook and agent-hook routes are matched by their own handlers: the
    // fallback would have answered `200` with the SPA for a shadowed route.
    for (uri, handler_marker) in [
        ("/webhooks/github", "webhooks disabled"),
        ("/agent-hooks/claude/x", "unknown agent hook token"),
    ] {
        let (status, _, body) = request(
            app.clone(),
            Method::POST,
            uri,
            Some("text/html"),
            Vec::new(),
        )
        .await;
        assert_ne!(status, StatusCode::OK, "{uri}: {status}");
        let body = body_text(&body);
        assert!(body.contains(handler_marker), "{uri}: {body}");
        assert!(!body.contains(EMBEDDED_MARKER), "{uri}");
    }
}

#[tokio::test]
async fn missing_assets_returns_a_diagnostic_not_500() {
    let dir = temp_dir("missing");
    let state = web_state(disk_config(&dir)).await;
    let app = web_router(&state);

    let (status, headers, bytes) = request(app, Method::GET, "/", None, Vec::new()).await;

    assert_eq!(status, StatusCode::OK);
    assert_ne!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(content_type(&headers), Some("text/html; charset=utf-8"));
    let body = body_text(&bytes);
    assert!(
        body.contains("web-dir") || body.contains("web/dist"),
        "{body}"
    );
}

#[tokio::test]
async fn web_disabled_serves_no_spa() {
    let config = FavettoConfig {
        web: crate::config::WebSettings {
            enabled: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let state = web_state(config).await;
    let app = crate::daemon::http_app(&state);

    let (root, _, _) = request(app.clone(), Method::GET, "/", None, Vec::new()).await;
    assert_eq!(root, StatusCode::NOT_FOUND);

    // The API is untouched by the disabled SPA.
    let (events, _, _) = request(app, Method::GET, "/events", Some("text/html"), Vec::new()).await;
    assert_eq!(events, StatusCode::UNAUTHORIZED);
}
