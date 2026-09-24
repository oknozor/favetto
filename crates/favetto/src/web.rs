//! Embedded SPA serving: `/`, `/assets/*`, and the SPA fallback.
//!
//! Assets are embedded with `rust-embed`; `[web].dir` / `--web-dir` overrides
//! them with a directory on disk for development. API routes are registered as
//! real routes on the daemon router, so the fallback can never shadow them.

use std::path::{Component, Path};
use std::sync::Arc;

use axum::extract::{Path as AxumPath, State as AxumState};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use rust_embed::RustEmbed;

use crate::state::State;

/// The `web/dist` assets built into the binary.
///
/// `allow_missing` keeps the crate buildable when the folder is absent (for
/// example a `cargo package` that omits `web/`); an empty asset set drives the
/// diagnostic page rather than a build failure.
#[derive(RustEmbed)]
#[folder = "../../web/dist"]
#[allow_missing = true]
struct Assets;

/// `GET /` and `GET /assets/{*path}`. The SPA fallback is attached by
/// `daemon::http_app`, so only one router owns a fallback.
pub fn routes() -> Router<Arc<State>> {
    Router::new()
        .route("/", get(index))
        .route("/assets/{*path}", get(asset))
}

/// `GET /`: the SPA shell, or a diagnostic page when nothing was embedded.
async fn index(AxumState(state): AxumState<Arc<State>>) -> Response {
    serve_index(&state).await
}

/// `GET /assets/{*path}`: a hashed asset with an explicit MIME type.
async fn asset(
    AxumState(state): AxumState<Arc<State>>,
    AxumPath(path): AxumPath<String>,
) -> Response {
    let rel = format!("assets/{path}");
    match load_asset(&state, &rel).await {
        Some(bytes) => asset_response(&rel, bytes),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// SPA fallback: an unmatched `GET` with `Accept: text/html` gets `index.html`;
/// anything else is a 404 (never `index.html`, never a 500).
pub async fn spa_fallback(
    AxumState(state): AxumState<Arc<State>>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    if method != Method::GET || !accepts_html(&headers) {
        return StatusCode::NOT_FOUND.into_response();
    }
    serve_index(&state).await
}

/// The SPA shell, or the diagnostic page when no `index.html` is available.
async fn serve_index(state: &State) -> Response {
    match load_asset(state, "index.html").await {
        Some(bytes) => html_response(bytes),
        None => diagnostic_response(),
    }
}

/// Load a `web/dist`-relative asset from disk (when `[web].dir` is set) or the
/// embedded set.
async fn load_asset(state: &State, rel: &str) -> Option<Vec<u8>> {
    if state.config.web.dir.as_os_str().is_empty() {
        Assets::get(rel).map(|file| file.data.into_owned())
    } else {
        read_disk_asset(&state.config.web.dir, rel).await
    }
}

/// Read `rel` below `root`, rejecting path components that could escape it.
async fn read_disk_asset(root: &Path, rel: &str) -> Option<Vec<u8>> {
    let escapes = Path::new(rel).components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    });
    if escapes {
        return None;
    }
    tokio::fs::read(root.join(rel)).await.ok()
}

/// The MIME type for an asset path, derived from its lowercased extension.
fn content_type(rel: &str) -> &'static str {
    let extension = rel
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase());
    match extension.as_deref() {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript",
        Some("css") => "text/css",
        Some("wasm") => "application/wasm",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("webmanifest") => "application/manifest+json",
        _ => "application/octet-stream",
    }
}

/// A `text/html` response that is never cached (the shell may change per build).
fn html_response(bytes: Vec<u8>) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        bytes,
    )
        .into_response()
}

/// An immutable, content-addressed asset response.
fn asset_response(rel: &str, bytes: Vec<u8>) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type(rel)),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        bytes,
    )
        .into_response()
}

/// A `200` HTML page explaining that no assets are embedded, instead of a 500.
fn diagnostic_response() -> Response {
    let body = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
        <title>favetto</title></head><body><main>\
        <h1>favetto</h1>\
        <p>No web client assets are embedded in this binary.</p>\
        <p>Build the client into <code>web/dist</code> before compiling, pass \
        <code>--web-dir</code>, or set <code>[web].dir</code> to serve assets \
        from disk during development.</p>\
        </main></body></html>";
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

/// Whether the request advertises `text/html` in its `Accept` header.
fn accepts_html(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("text/html"))
}

#[cfg(test)]
#[path = "web_tests.rs"]
mod tests;
