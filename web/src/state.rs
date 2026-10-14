//! Client state: connection status, credentials, cursor and caches.
//!
//! Everything the transport layer needs to survive a reload lives in
//! `localStorage` (token, daemon URL, event cursor); everything the views render
//! lives in Dioxus signals. The two are deliberately split:
//!
//! - [`Cache`] is a plain, signal-free value holding the task/event/catalog
//!   collections. It is pure so it can be unit-tested on the host without a
//!   Dioxus runtime, and so push handling cannot accidentally talk to the
//!   network.
//! - [`AppState`] wraps a [`Cache`] and the connection signals in `Copy` handles
//!   that are provided once at the root and read from every component.

use std::collections::HashMap;

use dioxus::prelude::*;
use favetto_wire::model::{Event, Task};
use favetto_wire::rpc::{push, CatalogEntry};
use uuid::Uuid;

/// `localStorage` key for the bearer token.
pub const TOKEN_KEY: &str = "favetto.token";
/// `localStorage` key for the daemon base URL (empty = same origin / unset).
pub const DAEMON_URL_KEY: &str = "favetto.daemon_url";
/// `localStorage` key for the last-seen durable event id (the resume cursor).
pub const CURSOR_KEY: &str = "favetto.event_cursor";

/// How many live events to keep in the in-memory feed.
pub const EVENT_CAP: usize = 500;

/// Whether the client is talking to the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// The last RPC/SSE attempt succeeded.
    Connected,
    /// No successful connection yet, or the caller explicitly reset it.
    Disconnected,
    /// The SSE stream dropped and is being retried with backoff.
    Reconnecting,
}

impl ConnectionState {
    /// A short label for the banner.
    pub fn label(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Disconnected => "disconnected — check the token and daemon URL",
            Self::Reconnecting => "reconnecting…",
        }
    }

    /// True only for [`ConnectionState::Connected`].
    pub fn is_connected(self) -> bool {
        matches!(self, Self::Connected)
    }
}

/// Which onboarding surface applies to the current credentials.
///
/// Derived purely from `(platform, daemon_url, token)` so the routing guard is
/// host-testable (see [`onboarding_step`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingStep {
    /// Android only: no daemon URL has been configured yet.
    DaemonUrl,
    /// A daemon URL is set (or the platform is same-origin web) but there is no
    /// bearer token yet.
    Pairing,
    /// Paired: the authenticated views can load.
    Ready,
}

/// The pure onboarding decision behind the route guards.
///
/// `require_daemon_url` is [`crate::config::daemon_url_required`]; it is passed
/// in (rather than read here) to keep the function testable and platform-free.
pub fn onboarding_step(
    require_daemon_url: bool,
    daemon_url: &str,
    token: Option<&str>,
) -> OnboardingStep {
    if require_daemon_url && daemon_url.trim().is_empty() {
        OnboardingStep::DaemonUrl
    } else if token.is_none_or(|t| t.trim().is_empty()) {
        OnboardingStep::Pairing
    } else {
        OnboardingStep::Ready
    }
}

/// The task/event/catalog collections, keyed and capped so pushes stay cheap.
#[derive(Debug, Default, Clone)]
pub struct Cache {
    /// Live tasks keyed by id. `tasks.list` seeds this; `task.updated` updates it.
    pub tasks: HashMap<Uuid, Task>,
    /// The live event feed, ascending by id, capped at [`EVENT_CAP`].
    pub events: Vec<Event>,
    /// The catalog, sorted by name as `catalog.list` returns it.
    pub catalog: Vec<CatalogEntry>,
    /// Highest durable event id seen; also the SSE resume cursor.
    pub cursor: Option<i64>,
}

impl Cache {
    /// Insert or replace a task. A `task.updated` push carries the summary.
    pub fn upsert_task(&mut self, task: Task) {
        self.tasks.insert(task.id, task);
    }

    /// Seed the task cache from `tasks.list`.
    pub fn set_tasks(&mut self, tasks: impl IntoIterator<Item = Task>) {
        self.tasks = tasks.into_iter().map(|t| (t.id, t)).collect();
    }

    /// Append a durable event, deduping by id and capping the feed.
    ///
    /// Replay and live delivery are at-least-once, so a duplicate id replaces
    /// the stored copy instead of appending. The cursor only ever moves forward.
    pub fn push_event(&mut self, event: Event) {
        if let Some(existing) = self.events.iter_mut().find(|e| e.id == event.id) {
            *existing = event.clone();
        } else {
            self.events.push(event.clone());
        }
        self.events.sort_by_key(|e| e.id);
        if self.events.len() > EVENT_CAP {
            let excess = self.events.len() - EVENT_CAP;
            self.events.drain(0..excess);
        }
        self.cursor = Some(self.cursor.map_or(event.id, |c| c.max(event.id)));
    }

    /// Feed a server push into the cache. Returns `true` when the cache changed.
    ///
    /// `catalog.updated` carries no payload; the caller re-fetches the catalog.
    pub fn apply_push(&mut self, method: &str, params: &serde_json::Value) -> bool {
        match method {
            push::EVENT => match serde_json::from_value::<Event>(params.clone()) {
                Ok(event) => {
                    self.push_event(event);
                    true
                }
                Err(_) => false,
            },
            push::TASK_UPDATED => match serde_json::from_value::<Task>(params.clone()) {
                Ok(task) => {
                    self.upsert_task(task);
                    true
                }
                Err(_) => false,
            },
            _ => false,
        }
    }
}

/// Shared client handles, provided once at the app root.
#[derive(Clone, Copy)]
pub struct AppState {
    /// Current connection state, rendered by the banner.
    pub connection: Signal<ConnectionState>,
    /// Bearer token, mirrored into `localStorage`.
    pub token: Signal<Option<String>>,
    /// Daemon base URL, mirrored into `localStorage`.
    pub daemon_url: Signal<String>,
    /// A `?pair=CODE` deep-link code captured before routing drops the query.
    /// Consumed once by the pairing form, then cleared.
    pub pending_pair_code: Signal<Option<String>>,
    /// Task/event/catalog caches plus the resume cursor.
    pub cache: Signal<Cache>,
    /// Bumped by a `catalog.updated` push so the Catalog view re-fetches.
    pub catalog_version: Signal<u64>,
}

/// Whether a push method should make the Catalog view re-fetch.
pub fn is_catalog_push(method: &str) -> bool {
    method == push::CATALOG_UPDATED
}

impl AppState {
    /// Build the initial state from `localStorage` (empty defaults on non-wasm).
    pub fn new() -> Self {
        let cursor = load_cursor();
        let cache = Cache {
            cursor,
            ..Cache::default()
        };
        Self {
            connection: Signal::new(ConnectionState::Disconnected),
            token: Signal::new(load_token()),
            daemon_url: Signal::new(load_daemon_url()),
            pending_pair_code: Signal::new(None),
            cache: Signal::new(cache),
            catalog_version: Signal::new(0),
        }
    }

    /// Persist a freshly exchanged token.
    pub fn set_token(&mut self, token: Option<String>) {
        save_token(token.as_deref());
        self.token.set(token);
    }

    /// Persist a user-configured daemon URL.
    pub fn set_daemon_url(&mut self, url: &str) {
        save_daemon_url(url);
        self.daemon_url.set(url.to_string());
    }

    /// Clear the bearer token (locally and in `localStorage`).
    pub fn clear_token(&mut self) {
        self.set_token(None);
    }

    /// Clear the daemon URL (locally and in `localStorage`).
    pub fn clear_daemon_url(&mut self) {
        self.set_daemon_url("");
    }

    /// Disconnect: forget both the token and the daemon URL. The route guard
    /// then sends the user back to `/pair`.
    pub fn forget(&mut self) {
        self.clear_token();
        self.clear_daemon_url();
    }

    /// Whether a bearer token is currently held.
    pub fn has_token(&self) -> bool {
        (self.token)().is_some()
    }

    /// Record the highest durable event id and persist it for the next resume.
    pub fn set_cursor(&mut self, cursor: i64) {
        save_cursor(cursor);
        self.cache.write().cursor = Some(cursor);
    }

    /// Signal a catalog change to the Catalog view.
    pub fn bump_catalog(&mut self) {
        let next = (self.catalog_version)() + 1;
        self.catalog_version.set(next);
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// localStorage
// ---------------------------------------------------------------------------

/// Read a string from `localStorage`; `None` when unset, on host, or on error
/// (private mode, disabled storage).
#[cfg(target_arch = "wasm32")]
pub fn load_string(key: &str) -> Option<String> {
    use gloo_storage::{LocalStorage, Storage};
    LocalStorage::get::<String>(key).ok()
}

/// Host stub: there is no `localStorage` outside the browser/WebView.
#[cfg(not(target_arch = "wasm32"))]
pub fn load_string(_key: &str) -> Option<String> {
    None
}

/// Write a string to `localStorage`, ignoring quota/privacy failures.
#[cfg(target_arch = "wasm32")]
pub fn save_string(key: &str, value: &str) {
    use gloo_storage::{LocalStorage, Storage};
    let _ = LocalStorage::set(key, value);
}

/// Host stub.
#[cfg(not(target_arch = "wasm32"))]
pub fn save_string(_key: &str, _value: &str) {}

/// Remove a key from `localStorage`.
#[cfg(target_arch = "wasm32")]
pub fn remove_key(key: &str) {
    use gloo_storage::{LocalStorage, Storage};
    LocalStorage::delete(key);
}

/// Host stub.
#[cfg(not(target_arch = "wasm32"))]
pub fn remove_key(_key: &str) {}

/// The stored bearer token.
pub fn load_token() -> Option<String> {
    load_string(TOKEN_KEY).filter(|t| !t.is_empty())
}

/// Persist (or clear) the bearer token.
pub fn save_token(token: Option<&str>) {
    match token {
        Some(token) if !token.is_empty() => save_string(TOKEN_KEY, token),
        _ => remove_key(TOKEN_KEY),
    }
}

/// The stored daemon URL (`""` = same origin).
pub fn load_daemon_url() -> String {
    load_string(DAEMON_URL_KEY).unwrap_or_default()
}

/// Persist the daemon URL; an empty value removes the key.
pub fn save_daemon_url(url: &str) {
    let url = url.trim();
    if url.is_empty() {
        remove_key(DAEMON_URL_KEY);
    } else {
        save_string(DAEMON_URL_KEY, url);
    }
}

/// The stored resume cursor.
pub fn load_cursor() -> Option<i64> {
    load_string(CURSOR_KEY).and_then(|v| v.trim().parse().ok())
}

/// Persist the resume cursor.
pub fn save_cursor(cursor: i64) {
    save_string(CURSOR_KEY, &cursor.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: i64) -> Event {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "kind": "task_finished",
            "payload": { "id": id },
            "created_at": "2026-01-01T00:00:00Z",
        }))
        .unwrap()
    }

    #[test]
    fn connection_state_labels_and_predicate() {
        assert!(ConnectionState::Connected.is_connected());
        assert!(!ConnectionState::Disconnected.is_connected());
        assert!(!ConnectionState::Reconnecting.is_connected());
        assert_eq!(ConnectionState::Connected.label(), "connected");
    }

    #[test]
    fn push_event_is_deduped_sorted_and_advances_the_cursor() {
        let mut cache = Cache::default();
        cache.push_event(event(3));
        cache.push_event(event(1));
        cache.push_event(event(2));
        // A duplicate replaces, it does not append.
        cache.push_event(event(2));
        let ids: Vec<i64> = cache.events.iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
        assert_eq!(cache.cursor, Some(3));
    }

    #[test]
    fn push_event_caps_the_feed_without_moving_the_cursor_back() {
        let mut cache = Cache::default();
        for id in 1..=(EVENT_CAP as i64 + 10) {
            cache.push_event(event(id));
        }
        assert_eq!(cache.events.len(), EVENT_CAP);
        assert_eq!(cache.events.first().unwrap().id, 11);
        assert_eq!(cache.cursor, Some(EVENT_CAP as i64 + 10));
    }

    #[test]
    fn apply_push_routes_event_and_task_updated() {
        let mut cache = Cache::default();
        let event_push = serde_json::to_value(event(7)).unwrap();
        assert!(cache.apply_push(push::EVENT, &event_push));
        assert_eq!(cache.cursor, Some(7));

        // Task deserialization is strict; an empty payload is ignored, not fatal.
        assert!(!cache.apply_push(push::TASK_UPDATED, &serde_json::json!({})));
        assert!(!cache.apply_push(push::CATALOG_UPDATED, &serde_json::json!({})));
    }

    #[test]
    fn is_catalog_push_matches_only_catalog_updated() {
        assert!(is_catalog_push(push::CATALOG_UPDATED));
        assert!(!is_catalog_push(push::EVENT));
        assert!(!is_catalog_push(push::TASK_UPDATED));
    }

    #[test]
    fn save_token_ignores_empty_values_on_host() {
        // On host the storage helpers are no-ops, so this only exercises the
        // empty/non-empty branch without panicking.
        save_token(None);
        save_token(Some(""));
        save_token(Some("secret"));
        assert_eq!(load_string(TOKEN_KEY), None);
    }

    #[test]
    fn onboarding_step_web_never_asks_for_a_url() {
        assert_eq!(onboarding_step(false, "", None), OnboardingStep::Pairing);
        assert_eq!(
            onboarding_step(false, "", Some("tok")),
            OnboardingStep::Ready
        );
        // A URL is irrelevant on web.
        assert_eq!(
            onboarding_step(false, "http://host:7878", None),
            OnboardingStep::Pairing
        );
    }

    #[test]
    fn onboarding_step_android_requires_a_url_first() {
        assert_eq!(onboarding_step(true, "", None), OnboardingStep::DaemonUrl);
        assert_eq!(
            onboarding_step(true, "   ", Some("tok")),
            OnboardingStep::DaemonUrl
        );
        assert_eq!(
            onboarding_step(true, "http://host:7878", None),
            OnboardingStep::Pairing
        );
        assert_eq!(
            onboarding_step(true, "http://host:7878", Some("tok")),
            OnboardingStep::Ready
        );
    }

    #[test]
    fn onboarding_step_treats_blank_tokens_as_absent() {
        assert_eq!(
            onboarding_step(false, "", Some("  ")),
            OnboardingStep::Pairing
        );
        assert_eq!(
            onboarding_step(false, "", Some("")),
            OnboardingStep::Pairing
        );
    }
}
