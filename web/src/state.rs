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
use favetto_wire::model::{Event, RepoInfo, Task};
use favetto_wire::rpc::{push, AgentEventPush, CatalogEntry};
use uuid::Uuid;

use crate::notifications::Permission;

/// `localStorage` key for the bearer token.
pub const TOKEN_KEY: &str = "favetto.token";
/// `localStorage` key for the daemon base URL (empty = same origin / unset).
pub const DAEMON_URL_KEY: &str = "favetto.daemon_url";
/// `localStorage` key for the last-seen durable event id (the resume cursor).
pub const CURSOR_KEY: &str = "favetto.event_cursor";
/// Platform state key for the persisted notification permission (Android).
pub const PERMISSION_KEY: &str = "favetto.notification_permission";

/// How many live events to keep in the in-memory feed.
pub const EVENT_CAP: usize = 500;

/// How many normalized agent events to keep per task feed.
pub const FEED_CAP: usize = 500;

/// One task's live agent-event feed, keyed by the resolved session.
///
/// Events arrive out of order relative to the backfill (`tasks.events`), so the
/// feed dedupes by `(session_id, seq)` and resets when the session changes (a
/// retry starts a new attempt).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct TaskFeed {
    /// The session the buffered events belong to; empty before one is resolved.
    pub session_id: String,
    /// Events ascending by `seq`.
    pub events: Vec<AgentEventPush>,
    /// Highest per-session `seq` applied (backfill cursor and dedupe key).
    pub last_seq: u64,
}

impl TaskFeed {
    /// Fold one event in, returning `true` when the feed changed.
    pub fn push(&mut self, event: AgentEventPush) -> bool {
        if !self.session_id.is_empty() && self.session_id != event.session_id {
            // A new session (retry/attempt boundary) replaces the old feed.
            *self = TaskFeed {
                session_id: event.session_id.clone(),
                ..TaskFeed::default()
            };
        } else if self.session_id.is_empty() {
            self.session_id = event.session_id.clone();
        }
        if event.seq <= self.last_seq {
            return false;
        }
        self.last_seq = event.seq;
        self.events.push(event);
        if self.events.len() > FEED_CAP {
            let excess = self.events.len() - FEED_CAP;
            self.events.drain(0..excess);
        }
        true
    }

    /// Merge a backfill window into the feed without dropping a live event that
    /// arrived while the request was in flight.
    pub fn merge(&mut self, window: TaskFeed) {
        if window.session_id.is_empty() {
            // No structured source: keep whatever the live feed already holds.
            return;
        }
        if self.session_id != window.session_id {
            *self = window;
            return;
        }
        for event in window.events {
            if !self.events.iter().any(|seen| seen.seq == event.seq) {
                self.events.push(event);
            }
        }
        self.events.sort_by_key(|event| event.seq);
        self.events.dedup_by_key(|event| event.seq);
        if self.events.len() > FEED_CAP {
            let excess = self.events.len() - FEED_CAP;
            self.events.drain(0..excess);
        }
        self.last_seq = self
            .events
            .last()
            .map_or(self.last_seq, |event| event.seq)
            .max(self.last_seq);
    }
}

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
    /// The known-repos registry, as `repos.list` returns it.
    pub repos: Vec<RepoInfo>,
    /// The daemon-wide active repo name; `None` means the global view.
    pub active_repo: Option<String>,
    /// Highest durable event id seen; also the SSE resume cursor.
    pub cursor: Option<i64>,
    /// Per-task normalized agent-event feeds, keyed by task id.
    pub feeds: HashMap<Uuid, TaskFeed>,
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
            push::AGENT_EVENT => match serde_json::from_value::<AgentEventPush>(params.clone()) {
                Ok(event) => self.upsert_feed_event(event),
                Err(_) => false,
            },
            _ => false,
        }
    }

    /// Store the fetched repo registry and active repo (`repos.list`).
    pub fn set_repos(&mut self, repos: Vec<RepoInfo>, active: Option<String>) {
        self.repos = repos;
        self.active_repo = active;
    }

    /// Replace the cached catalog (`catalog.list`).
    ///
    /// Task rows resolve their metadata chips from this, so the catalog is
    /// fetched alongside `tasks.list` and degrades to no chips when absent.
    pub fn set_catalog(&mut self, catalog: Vec<CatalogEntry>) {
        self.catalog = catalog;
    }

    /// The cached catalog entry for a task's storage name, if the catalog has
    /// been loaded. A task and its catalog definition share the same identity
    /// (`repo:<name>/<path>` for a repo-local task, a bare name otherwise).
    pub fn catalog_entry(&self, name: &str) -> Option<&CatalogEntry> {
        self.catalog.iter().find(|entry| entry.name == name)
    }

    /// Whether a row's repo namespace is in scope. With no active repo every
    /// row is visible; otherwise only rows belonging to the active repo are.
    ///
    /// Mirrors `favetto_tui`'s `App::repo_visible`.
    pub fn repo_visible(&self, repo: Option<&str>) -> bool {
        match &self.active_repo {
            Some(active) => repo == Some(active.as_str()),
            None => true,
        }
    }

    /// The repo a persisted event belongs to, derived from its payload.
    ///
    /// Task-lifecycle events carry `name`; the terminal/cron ones carry only
    /// `task_id`, resolved against the known task list. `None` means a global
    /// row (integration events, or a task we cannot resolve).
    pub fn event_repo(&self, event: &Event) -> Option<String> {
        if let Some(name) = event.payload.get("name").and_then(|v| v.as_str()) {
            return task_repo(name).map(str::to_string);
        }
        let task_id = event.payload.get("task_id").and_then(|v| v.as_str())?;
        let task_id = Uuid::parse_str(task_id).ok()?;
        let task = self.tasks.values().find(|t| t.id == task_id)?;
        task_repo(&task.name).map(str::to_string)
    }

    /// The task rows visible under the active repo.
    pub fn visible_tasks(&self) -> Vec<&Task> {
        self.tasks
            .values()
            .filter(|t| self.repo_visible(task_repo(&t.name)))
            .collect()
    }

    /// The event rows visible under the active repo, ascending by id.
    pub fn visible_events(&self) -> Vec<&Event> {
        self.events
            .iter()
            .filter(|e| self.repo_visible(self.event_repo(e).as_deref()))
            .collect()
    }

    /// Fold one `agent.event` push into the task feed. Returns `false` for a
    /// payload with no resolvable `task_id` (the feed is keyed by task) or a
    /// duplicate/stale `seq`.
    pub fn upsert_feed_event(&mut self, event: AgentEventPush) -> bool {
        let Some(task_id) = event
            .task_id
            .as_deref()
            .and_then(|id| Uuid::parse_str(id).ok())
        else {
            return false;
        };
        self.feeds.entry(task_id).or_default().push(event)
    }

    /// Replace (or merge) a task's feed from the `tasks.events` backfill.
    pub fn set_feed(&mut self, task: Uuid, feed: TaskFeed) {
        self.feeds.entry(task).or_default().merge(feed);
    }
}

/// Reserved prefix for repo-qualified task identities (`repo:<name>/<path>`).
///
/// Mirrors `favetto_core::tasks::REPO_PREFIX`. The web client links only
/// `favetto-wire`, so [`task_repo`] re-derives the split here.
pub const REPO_PREFIX: &str = "repo:";

/// The repo namespace of a repo-qualified task name.
///
/// Returns `None` for a bare (global) name, or anything that does not start
/// with [`REPO_PREFIX`] followed by non-empty `repo/relative-path` halves.
pub fn task_repo(name: &str) -> Option<&str> {
    let rest = name.strip_prefix(REPO_PREFIX)?;
    let (repo, rel) = rest.split_once('/')?;
    if repo.is_empty() || rel.is_empty() {
        None
    } else {
        Some(repo)
    }
}

/// The display name for a task: the repo prefix is stripped from a
/// repo-qualified identity, leaving the bare relative path.
///
/// `repo:favetto/triage` → `triage` and `repo:web/pipelines/plan` →
/// `pipelines/plan`. Bare/global names (`triage`) and malformed repo prefixes
/// are returned unchanged, so this never hides a name it cannot split.
pub fn task_display_name(name: &str) -> &str {
    match task_repo(name) {
        Some(_) => name
            .strip_prefix(REPO_PREFIX)
            .and_then(|rest| rest.split_once('/'))
            .map(|(_, rel)| rel)
            .unwrap_or(name),
        None => name,
    }
}

/// The display label for a repo: its name, or `global` for an unscoped row.
pub fn repo_label(repo: Option<&str>) -> &str {
    repo.unwrap_or("global")
}

/// The status-bar label for the active repo.
pub fn active_repo_label(active: Option<&str>) -> String {
    format!("repo: {}", repo_label(active))
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
    /// Browser/WebView notification permission, mirrored from `Notification.permission`.
    pub notification_permission: Signal<Permission>,
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
            notification_permission: Signal::new(load_permission()),
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
// Persisted state (localStorage on web, files on native)
// ---------------------------------------------------------------------------

/// Read a string from `localStorage`; `None` when unset, on host, or on error
/// (private mode, disabled storage).
#[cfg(target_arch = "wasm32")]
pub fn load_string(key: &str) -> Option<String> {
    use gloo_storage::{LocalStorage, Storage};
    LocalStorage::get::<String>(key).ok()
}

/// Native build: read from the platform state directory (`crate::storage`).
#[cfg(not(target_arch = "wasm32"))]
pub fn load_string(key: &str) -> Option<String> {
    crate::storage::load_string(key)
}

/// Write a string to `localStorage`, ignoring quota/privacy failures.
#[cfg(target_arch = "wasm32")]
pub fn save_string(key: &str, value: &str) {
    use gloo_storage::{LocalStorage, Storage};
    let _ = LocalStorage::set(key, value);
}

/// Native build: write to the platform state directory.
#[cfg(not(target_arch = "wasm32"))]
pub fn save_string(key: &str, value: &str) {
    crate::storage::save_string(key, value);
}

/// Remove a key from `localStorage`.
#[cfg(target_arch = "wasm32")]
pub fn remove_key(key: &str) {
    use gloo_storage::{LocalStorage, Storage};
    LocalStorage::delete(key);
}

/// Native build: remove the key from the platform state directory.
#[cfg(not(target_arch = "wasm32"))]
pub fn remove_key(key: &str) {
    crate::storage::remove_key(key);
}

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

/// The persisted notification permission (`NotRequested` when unset/unknown).
///
/// Only the Android path writes this key; on web `load_string` reads
/// `localStorage`, where nothing is ever stored, so it stays `NotRequested` and
/// the app's mount effect corrects it from the live browser value.
pub fn load_permission() -> Permission {
    load_string(PERMISSION_KEY)
        .as_deref()
        .map(Permission::parse)
        .unwrap_or_default()
}

/// Persist the notification permission.
pub fn save_permission(permission: Permission) {
    save_string(PERMISSION_KEY, permission.as_str());
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

    fn feed_push(session: &str, task: Uuid, seq: u64) -> serde_json::Value {
        serde_json::to_value(favetto_wire::rpc::AgentEventPush {
            session_id: session.to_string(),
            task_id: Some(task.to_string()),
            seq,
            event: favetto_wire::model::AgentStateEvent::TurnStarted,
        })
        .unwrap()
    }

    #[test]
    fn agent_event_push_dedupes_and_advances_the_feed() {
        let mut cache = Cache::default();
        let task = Uuid::new_v4();
        assert!(cache.apply_push(push::AGENT_EVENT, &feed_push("ses_1", task, 1)));
        assert!(cache.apply_push(push::AGENT_EVENT, &feed_push("ses_1", task, 2)));
        // A duplicate seq is ignored.
        assert!(!cache.apply_push(push::AGENT_EVENT, &feed_push("ses_1", task, 2)));
        // A stale seq is ignored.
        assert!(!cache.apply_push(push::AGENT_EVENT, &feed_push("ses_1", task, 1)));
        let feed = cache.feeds.get(&task).expect("feed");
        assert_eq!(feed.session_id, "ses_1");
        assert_eq!(feed.last_seq, 2);
        assert_eq!(feed.events.len(), 2);
        // The durable cursor is untouched by a non-durable agent push.
        assert_eq!(cache.cursor, None);
    }

    #[test]
    fn agent_event_push_resets_on_a_new_session() {
        let mut cache = Cache::default();
        let task = Uuid::new_v4();
        cache.apply_push(push::AGENT_EVENT, &feed_push("ses_1", task, 5));
        cache.apply_push(push::AGENT_EVENT, &feed_push("ses_2", task, 1));
        let feed = cache.feeds.get(&task).expect("feed");
        assert_eq!(feed.session_id, "ses_2");
        assert_eq!(feed.events.len(), 1);
        assert_eq!(feed.events[0].seq, 1);
        assert_eq!(feed.last_seq, 1);
    }

    #[test]
    fn agent_event_feed_is_capped_and_ignores_missing_task_id() {
        let mut cache = Cache::default();
        let task = Uuid::new_v4();
        for seq in 1..=(FEED_CAP as u64 + 5) {
            cache.apply_push(push::AGENT_EVENT, &feed_push("ses_1", task, seq));
        }
        let feed = cache.feeds.get(&task).expect("feed");
        assert_eq!(feed.events.len(), FEED_CAP);
        assert_eq!(feed.events.first().unwrap().seq, 6);
        assert_eq!(feed.last_seq, FEED_CAP as u64 + 5);

        // A payload with no `task_id` cannot be keyed.
        let orphan = serde_json::to_value(favetto_wire::rpc::AgentEventPush {
            session_id: "ses_1".to_string(),
            task_id: None,
            seq: 1,
            event: favetto_wire::model::AgentStateEvent::TurnStarted,
        })
        .unwrap();
        assert!(!cache.apply_push(push::AGENT_EVENT, &orphan));
    }

    #[test]
    fn set_feed_merges_backfill_without_dropping_live_events() {
        let mut cache = Cache::default();
        let task = Uuid::new_v4();
        // A live event arrives before the backfill response.
        cache.apply_push(push::AGENT_EVENT, &feed_push("ses_1", task, 3));
        cache.set_feed(
            task,
            TaskFeed {
                session_id: "ses_1".to_string(),
                events: vec![
                    serde_json::from_value(feed_push("ses_1", task, 1)).unwrap(),
                    serde_json::from_value(feed_push("ses_1", task, 2)).unwrap(),
                ],
                last_seq: 2,
            },
        );
        let feed = cache.feeds.get(&task).expect("feed");
        let seqs: Vec<u64> = feed.events.iter().map(|event| event.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
        assert_eq!(feed.last_seq, 3);
    }

    #[test]
    fn is_catalog_push_matches_only_catalog_updated() {
        assert!(is_catalog_push(push::CATALOG_UPDATED));
        assert!(!is_catalog_push(push::EVENT));
        assert!(!is_catalog_push(push::TASK_UPDATED));
    }

    #[test]
    fn save_token_ignores_empty_values_on_host() {
        // `FAVETTO_WEB_STATE_DIR` is process-global; another test may set it
        // concurrently, which would make the host "no state dir" assumption
        // false and let `save_token` persist. Serialize with the tests that
        // mutate it (see `storage::TEST_STATE_DIR_LOCK`).
        let _guard = crate::storage::TEST_STATE_DIR_LOCK.lock().unwrap();
        // On host the storage helpers are no-ops, so this only exercises the
        // empty/non-empty branch without panicking.
        save_token(None);
        save_token(Some(""));
        save_token(Some("secret"));
        assert_eq!(load_string(TOKEN_KEY), None);
    }

    /// The Android permission survives a restart: it is written through the same
    /// native storage as the token and read back on the next launch.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn permission_persists_through_the_state_dir_override() {
        // `FAVETTO_WEB_STATE_DIR` is process-global; serialize with the storage
        // round-trip test.
        let _guard = crate::storage::TEST_STATE_DIR_LOCK.lock().unwrap();
        let dir =
            std::env::temp_dir().join(format!("favetto-web-permission-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("FAVETTO_WEB_STATE_DIR", &dir);

        assert_eq!(load_permission(), Permission::NotRequested);
        save_permission(Permission::Granted);
        assert_eq!(load_permission(), Permission::Granted);
        save_permission(Permission::Denied);
        assert_eq!(load_permission(), Permission::Denied);

        std::env::remove_var("FAVETTO_WEB_STATE_DIR");
        let _ = std::fs::remove_dir_all(&dir);
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

    fn repo(name: &str, path: &str, available: bool, active: bool) -> RepoInfo {
        RepoInfo {
            name: name.to_string(),
            path: path.to_string(),
            available,
            active,
            source: favetto_wire::model::RepoSource::Manual,
        }
    }

    fn make_task(name: &str) -> Task {
        serde_json::from_value(serde_json::json!({
            "id": Uuid::new_v4(),
            "name": name,
            "status": "pending",
            "attempt": 0,
            "input": {},
            "output": null,
            "dedupe_key": null,
            "created_at": "2026-01-01T00:00:00Z",
            "started_at": null,
            "finished_at": null,
            "error": null,
            "failure": null,
            "session_id": null,
            "session_title": null,
            "parent_id": null,
            "root_id": null,
            "interactive": false,
        }))
        .unwrap()
    }

    fn event_with(id: i64, kind: &str, payload: serde_json::Value) -> Event {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "kind": kind,
            "payload": payload,
            "created_at": "2026-01-01T00:00:00Z",
        }))
        .unwrap()
    }

    #[test]
    fn task_repo_splits_only_well_formed_qualified_names() {
        assert_eq!(task_repo("repo:favetto/triage"), Some("favetto"));
        assert_eq!(task_repo("repo:web/pipelines/plan"), Some("web"));
        assert_eq!(task_repo("triage"), None);
        assert_eq!(task_repo("pipelines/plan"), None);
        assert_eq!(task_repo("repo:"), None);
        assert_eq!(task_repo("repo:noslash"), None);
        assert_eq!(task_repo("repo:/rel"), None);
        assert_eq!(task_repo("repo:name/"), None);
    }

    #[test]
    fn task_display_name_strips_only_well_formed_prefixes() {
        assert_eq!(task_display_name("repo:favetto/triage"), "triage");
        assert_eq!(
            task_display_name("repo:web/pipelines/plan"),
            "pipelines/plan"
        );
        // Bare/global names are untouched.
        assert_eq!(task_display_name("triage"), "triage");
        assert_eq!(task_display_name("pipelines/plan"), "pipelines/plan");
        // Malformed prefixes are left as-is rather than losing the name.
        assert_eq!(task_display_name("repo:"), "repo:");
        assert_eq!(task_display_name("repo:noslash"), "repo:noslash");
        assert_eq!(task_display_name("repo:/rel"), "repo:/rel");
        assert_eq!(task_display_name("repo:name/"), "repo:name/");
    }

    #[test]
    fn set_catalog_replaces_and_looks_up_by_task_name() {
        let mut cache = Cache::default();
        let entry: CatalogEntry = serde_json::from_value(serde_json::json!({
            "name": "repo:favetto/triage",
            "agent": "claude",
            "provider": null,
            "model": null,
            "cwd": null,
            "schedule": null,
            "needs": null,
            "vars": [],
            "prompt": "",
            "repo": "favetto",
        }))
        .unwrap();
        cache.set_catalog(vec![entry]);
        assert_eq!(cache.catalog.len(), 1);
        assert_eq!(
            cache
                .catalog_entry("repo:favetto/triage")
                .map(|e| e.agent.as_deref()),
            Some(Some("claude"))
        );
        assert!(cache.catalog_entry("triage").is_none());

        // Replacing with an empty catalog clears the lookup.
        cache.set_catalog(Vec::new());
        assert!(cache.catalog_entry("repo:favetto/triage").is_none());
    }

    #[test]
    fn active_repo_label_defaults_to_global() {
        assert_eq!(active_repo_label(None), "repo: global");
        assert_eq!(active_repo_label(Some("favetto")), "repo: favetto");
        assert_eq!(repo_label(None), "global");
        assert_eq!(repo_label(Some("favetto")), "favetto");
    }

    #[test]
    fn repo_visible_scopes_rows_to_the_active_repo() {
        let mut cache = Cache::default();
        assert!(cache.repo_visible(None));
        assert!(cache.repo_visible(Some("a")));

        cache.set_repos(
            vec![repo("a", "/code/a", true, true)],
            Some("a".to_string()),
        );
        assert!(cache.repo_visible(Some("a")));
        assert!(!cache.repo_visible(Some("b")));
        assert!(!cache.repo_visible(None));
    }

    #[test]
    fn visible_tasks_filters_by_active_repo() {
        let mut cache = Cache::default();
        cache.set_tasks(vec![
            make_task("global_task"),
            make_task("repo:a/one"),
            make_task("repo:b/two"),
        ]);
        // The global view lists everything.
        assert_eq!(cache.visible_tasks().len(), 3);

        cache.active_repo = Some("a".to_string());
        let visible = cache.visible_tasks();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].name, "repo:a/one");
        assert_eq!(task_repo(&visible[0].name), Some("a"));
        assert_eq!(repo_label(task_repo(&visible[0].name)), "a");
    }

    #[test]
    fn event_repo_derives_from_name_and_task_id() {
        let mut cache = Cache::default();
        let local = make_task("repo:a/build");
        let local_id = local.id.to_string();
        cache.set_tasks(vec![local]);

        // Lifecycle events carry `name` directly.
        let named = event_with(
            1,
            "task_started",
            serde_json::json!({ "name": "repo:a/build" }),
        );
        assert_eq!(cache.event_repo(&named).as_deref(), Some("a"));

        // A bare name is global.
        let global_named = event_with(2, "task_started", serde_json::json!({ "name": "build" }));
        assert_eq!(cache.event_repo(&global_named), None);

        // Terminal/cron events carry only `task_id`, resolved against the list.
        let by_id = event_with(
            3,
            "task_completed",
            serde_json::json!({ "task_id": local_id }),
        );
        assert_eq!(cache.event_repo(&by_id).as_deref(), Some("a"));

        // An unresolved task id and an integration event both fall back to global.
        let unknown = event_with(
            4,
            "task_failed",
            serde_json::json!({ "task_id": Uuid::new_v4().to_string() }),
        );
        assert_eq!(cache.event_repo(&unknown), None);
        let integration = event_with(
            5,
            "issue_created",
            serde_json::json!({ "repo": "owner/name" }),
        );
        assert_eq!(cache.event_repo(&integration), None);
    }

    #[test]
    fn visible_events_filters_by_active_repo() {
        let mut cache = Cache::default();
        let local = make_task("repo:a/build");
        let local_id = local.id.to_string();
        cache.set_tasks(vec![local]);
        cache.push_event(event_with(
            1,
            "task_started",
            serde_json::json!({ "name": "repo:a/build" }),
        ));
        cache.push_event(event_with(
            2,
            "task_started",
            serde_json::json!({ "name": "global_task" }),
        ));
        cache.push_event(event_with(
            3,
            "task_completed",
            serde_json::json!({ "task_id": local_id }),
        ));

        assert_eq!(cache.visible_events().len(), 3);
        cache.active_repo = Some("a".to_string());
        let ids: Vec<i64> = cache.visible_events().iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![1, 3]);
    }

    #[test]
    fn set_repos_replaces_registry_and_active_repo() {
        let mut cache = Cache::default();
        cache.set_repos(
            vec![
                repo("a", "/code/a", true, true),
                repo("b", "/code/b", false, false),
            ],
            Some("a".to_string()),
        );
        assert_eq!(cache.repos.len(), 2);
        assert_eq!(cache.active_repo.as_deref(), Some("a"));
        cache.set_repos(Vec::new(), None);
        assert!(cache.repos.is_empty());
        assert_eq!(cache.active_repo, None);
    }
}
