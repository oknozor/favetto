//! Browser/WebView notifications for commands that need a human.
//!
//! On web (`wasm32`) this uses the browser `Notification` API. On Android the
//! WebView does not implement that API and the platform backend has no
//! notification permission, so the native path goes through JNI instead (see
//! `src/android_notify.rs`): it requests `POST_NOTIFICATIONS`, posts a real
//! system notification, and reads the tap deep link. Background paging when the
//! app is closed is handled by the daemon's `ntfy` channel, not here.
//!
//! The permission enum and the event→alert extraction are pure and unit-tested
//! on the host; only the target-gated functions below talk to the platform.

use favetto_wire::model::{Event, EventKind};

/// The browser's notification permission, mirrored into [`crate::state::AppState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Permission {
    /// The user has not been asked yet (browser `default`).
    #[default]
    NotRequested,
    /// Notifications are allowed.
    Granted,
    /// The user refused (or the OS refused) notifications.
    Denied,
}

impl Permission {
    /// A short human label for the Settings view.
    pub fn label(self) -> &'static str {
        match self {
            Permission::NotRequested => "not requested",
            Permission::Granted => "granted",
            Permission::Denied => "denied",
        }
    }

    /// True only when notifications may be shown.
    pub fn is_granted(self) -> bool {
        matches!(self, Permission::Granted)
    }

    /// True when the client should ask the browser for permission.
    pub fn needs_request(self) -> bool {
        matches!(self, Permission::NotRequested)
    }

    /// Stable token persisted to storage.
    pub fn as_str(self) -> &'static str {
        match self {
            Permission::NotRequested => "not_requested",
            Permission::Granted => "granted",
            Permission::Denied => "denied",
        }
    }

    /// Parse a persisted token; unknown/absent values fall back to `NotRequested`.
    pub fn parse(raw: &str) -> Permission {
        match raw {
            "granted" => Permission::Granted,
            "denied" => Permission::Denied,
            _ => Permission::NotRequested,
        }
    }
}

/// Deterministic 31-bit id for `NotificationManager.notify`, derived from the
/// task id (FNV-1a). Stable within and across runs so a re-notify replaces the
/// previous notification for the same task.
pub fn notification_id(task_id: &str) -> i32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in task_id.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    (hash & 0x7fff_ffff) as i32
}

/// A system-notification alert derived from a durable event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    /// Notification title (the task name).
    pub title: String,
    /// Notification body (the agent's prompt or the run summary).
    pub body: String,
    /// The task to open when the notification is clicked.
    pub task_id: String,
}

/// The task-event kinds the Settings card lets the user subscribe to.
///
/// Mirrors the TUI sound design (`favetto_tui::tui::App::cue_for_event`): only
/// `task_finished` is surfaced for a terminal run, with success/failure derived
/// from the payload, so `task_completed` / `task_failed` never double up.
/// `task_awaiting_input` keeps its pre-existing behavior.
pub const NOTIFIABLE_KINDS: &[EventKind] = &[EventKind::TaskAwaitingInput, EventKind::TaskFinished];

/// A short Settings label for a notifiable event kind.
pub fn event_label(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::TaskAwaitingInput => "Task needs input",
        EventKind::TaskFinished => "Task finished",
        _ => kind.as_str(),
    }
}

/// The set of task-event kinds that raise a system notification.
///
/// Persisted as a comma-separated list of wire names; the empty set is encoded
/// as `none` because storage treats an empty string as "unset" (falling back to
/// [`Subscription::default`]). Defaults to `task_awaiting_input`, matching the
/// behavior before the set became configurable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    kinds: Vec<EventKind>,
}

impl Default for Subscription {
    fn default() -> Self {
        Self {
            kinds: vec![EventKind::TaskAwaitingInput],
        }
    }
}

impl Subscription {
    /// The subscribed kinds in canonical ([`NOTIFIABLE_KINDS`]) order.
    pub fn kinds(&self) -> Vec<EventKind> {
        NOTIFIABLE_KINDS
            .iter()
            .filter(|kind| self.contains(kind))
            .cloned()
            .collect()
    }

    /// True when `kind` is subscribed.
    pub fn contains(&self, kind: &EventKind) -> bool {
        self.kinds.iter().any(|subscribed| subscribed == kind)
    }

    /// True when no event kind is subscribed (notifications are off).
    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty()
    }

    /// Subscribe or unsubscribe a kind. Unknown/unsupported kinds are ignored.
    pub fn set(&mut self, kind: &EventKind, subscribed: bool) {
        if !NOTIFIABLE_KINDS.contains(kind) {
            return;
        }
        let present = self.contains(kind);
        if subscribed && !present {
            self.kinds.push(kind.clone());
        } else if !subscribed && present {
            self.kinds.retain(|subscribed| subscribed != kind);
        }
    }

    /// Stable token persisted to storage (`none` for the empty set).
    pub fn as_str(&self) -> String {
        let names: Vec<&'static str> = NOTIFIABLE_KINDS
            .iter()
            .filter(|kind| self.contains(kind))
            .map(|kind| kind.as_str())
            .collect();
        if names.is_empty() {
            "none".to_string()
        } else {
            names.join(",")
        }
    }

    /// Parse a persisted token. Unknown/unsupported names and blanks are
    /// ignored; an empty or `none` value yields the empty set.
    pub fn parse(raw: &str) -> Subscription {
        let mut kinds = Vec::new();
        for token in raw.split(',') {
            let token = token.trim();
            if token.is_empty() || token == "none" {
                continue;
            }
            if let Some(kind) = EventKind::from_name(token) {
                if NOTIFIABLE_KINDS.contains(&kind) && !kinds.contains(&kind) {
                    kinds.push(kind);
                }
            }
        }
        Subscription { kinds }
    }
}

/// Build an [`Alert`] from a durable event, or `None` for a kind that does not
/// raise a notification.
///
/// - `task_awaiting_input` payloads are emitted by
///   `crates/favetto/src/attention.rs::mark_awaiting`: `{ task_id, name, session_id, reason }`
///   with `reason.message` carrying the prompt.
/// - `task_finished` payloads are emitted by
///   `crates/favetto/src/executor.rs::finished_payload`: `{ name, task_id, success, status, attempt, retryable, summary }`.
pub fn alert_for(event: &Event) -> Option<Alert> {
    match event.kind {
        EventKind::TaskAwaitingInput => awaiting_alert(event),
        EventKind::TaskFinished => finished_alert(event),
        _ => None,
    }
}

/// `task_awaiting_input`: title `<name> needs input`, body the prompt.
fn awaiting_alert(event: &Event) -> Option<Alert> {
    let task_id = event.payload.get("task_id")?.as_str()?.to_string();
    let name = event
        .payload
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("task");
    let message = event
        .payload
        .get("reason")
        .and_then(|reason| reason.get("message"))
        .and_then(|v| v.as_str())
        .unwrap_or("needs your input");
    Some(Alert {
        title: format!("{name} needs input"),
        body: message.to_string(),
        task_id,
    })
}

/// `task_finished`: title `<name> finished` / `<name> failed`, body the bounded
/// run `summary` (falling back to a generic line when the payload has none).
fn finished_alert(event: &Event) -> Option<Alert> {
    let task_id = event.payload.get("task_id")?.as_str()?.to_string();
    let name = event
        .payload
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("task");
    let success = event
        .payload
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let title = if success {
        format!("{name} finished")
    } else {
        format!("{name} failed")
    };
    let body = event
        .payload
        .get("summary")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|summary| !summary.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            if success {
                "finished successfully".to_string()
            } else {
                "finished with an error".to_string()
            }
        });
    Some(Alert {
        title,
        body,
        task_id,
    })
}

/// The in-app route a notification click opens (matches the `ntfy` deep link).
pub fn deep_link(task_id: &str) -> String {
    format!("/inbox/{task_id}")
}

/// Whether to surface a notification: permission granted and the app is visible.
///
/// Matching the issue, notifications are only shown while the tab/WebView is in
/// the foreground (the in-app Inbox already shows the awaiting card when focused).
pub fn should_notify(permission: Permission, hidden: bool) -> bool {
    permission.is_granted() && !hidden
}

/// True when the WebView/browser actually exposes the `Notification` global.
#[cfg(target_arch = "wasm32")]
fn has_notification_api() -> bool {
    web_sys::window()
        .map(|window| {
            js_sys::Reflect::has(&window, &wasm_bindgen::JsValue::from_str("Notification"))
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

/// The current permission; `NotRequested` on host or when the API is absent.
#[cfg(target_arch = "wasm32")]
pub fn current_permission() -> Permission {
    if !has_notification_api() {
        return Permission::NotRequested;
    }
    match web_sys::Notification::permission() {
        web_sys::NotificationPermission::Granted => Permission::Granted,
        web_sys::NotificationPermission::Denied => Permission::Denied,
        _ => Permission::NotRequested,
    }
}

/// Android: read the live `POST_NOTIFICATIONS` state so a permission granted or
/// revoked outside the app is reflected. The OS is authoritative when reachable;
/// a not-granted OS state only overrides a stale persisted `Granted`, so a
/// not-yet-asked install keeps its `NotRequested` (and still auto-requests).
#[cfg(target_os = "android")]
pub fn current_permission() -> Permission {
    let stored = crate::state::load_permission();
    match crate::android_notify::permission_state() {
        Some(Permission::Granted) => Permission::Granted,
        Some(Permission::Denied) if stored == Permission::Granted => Permission::Denied,
        Some(_) => stored,
        None => stored,
    }
}

/// Host stub: there is no browser here.
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
pub fn current_permission() -> Permission {
    Permission::NotRequested
}

/// Ask the browser/WebView for permission and return the resolved state.
#[cfg(target_arch = "wasm32")]
pub async fn request_permission() -> Permission {
    if !has_notification_api() {
        return Permission::NotRequested;
    }
    if let Ok(promise) = web_sys::Notification::request_permission() {
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }
    current_permission()
}

/// Android: ask for `POST_NOTIFICATIONS` through the Activity, then poll the OS
/// until the user answers, and persist the result.
#[cfg(target_os = "android")]
pub async fn request_permission() -> Permission {
    // A prior persisted `Denied` means we already asked; the platform can use
    // that to settle without waiting when the dialog will not be shown again.
    let asked_before = crate::state::load_permission() == Permission::Denied;
    let next = crate::android_notify::request_notification_permission(asked_before).await;
    crate::state::save_permission(next);
    next
}

/// Host stub.
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
pub async fn request_permission() -> Permission {
    Permission::NotRequested
}

/// True when the document/tab is hidden (backgrounded).
#[cfg(target_arch = "wasm32")]
pub fn document_hidden() -> bool {
    web_sys::window()
        .and_then(|window| window.document())
        .map(|document| document.hidden())
        .unwrap_or(false)
}

/// Android: the WebView is foreground whenever the app is; background paging is
/// the daemon's `ntfy` channel.
#[cfg(target_os = "android")]
pub fn document_hidden() -> bool {
    false
}

/// Host stub.
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
pub fn document_hidden() -> bool {
    false
}

/// Show a notification that opens `/inbox/{task_id}` when clicked.
#[cfg(target_arch = "wasm32")]
pub fn notify(title: &str, body: &str, task_id: &str) {
    use wasm_bindgen::prelude::Closure;
    use wasm_bindgen::JsCast;

    if !has_notification_api() {
        return;
    }
    // Use the `set_*` setters, not web-sys' deprecated builder methods, so the
    // web job's `-D warnings` stays clean.
    let options = web_sys::NotificationOptions::new();
    options.set_body(body);
    options.set_tag(task_id);
    options.set_data(&wasm_bindgen::JsValue::from_str(task_id));
    let Ok(notification) = web_sys::Notification::new_with_options(title, &options) else {
        return;
    };
    let url = deep_link(task_id);
    let onclick = Closure::<dyn FnMut()>::once_into_js(move || {
        if let Some(window) = web_sys::window() {
            let _ = window.location().set_href(&url);
        }
    });
    notification.set_onclick(Some(onclick.unchecked_ref::<js_sys::Function>()));
}

/// Android: post a real system notification via `NotificationManager`.
#[cfg(target_os = "android")]
pub fn notify(title: &str, body: &str, task_id: &str) {
    crate::android_notify::post_notification(title, body, task_id);
}

/// Android: the task id carried by the launch/`onNewIntent` intent (if any),
/// cleared so it is consumed once.
#[cfg(target_os = "android")]
pub fn take_pending_deep_link() -> Option<String> {
    crate::android_notify::take_pending_deep_link()
}

/// Host stub.
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
pub fn notify(_title: &str, _body: &str, _task_id: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: &str, payload: serde_json::Value) -> Event {
        serde_json::from_value(serde_json::json!({
            "id": 1,
            "kind": kind,
            "payload": payload,
            "created_at": "2026-01-01T00:00:00Z",
        }))
        .expect("event")
    }

    fn awaiting_payload(reason: Option<serde_json::Value>) -> serde_json::Value {
        let mut payload = serde_json::json!({
            "task_id": "task-123",
            "name": "implement_issue",
            "session_id": "sess-1",
        });
        if let Some(reason) = reason {
            payload["reason"] = reason;
        }
        payload
    }

    fn finished_payload(success: bool, summary: Option<&str>) -> serde_json::Value {
        let mut payload = serde_json::json!({
            "name": "implement_issue",
            "task_id": "task-123",
            "success": success,
            "status": if success { "completed" } else { "failed" },
            "attempt": 1,
            "retryable": !success,
        });
        if let Some(summary) = summary {
            payload["summary"] = serde_json::Value::String(summary.to_string());
        }
        payload
    }

    #[test]
    fn alert_for_extracts_the_awaiting_name_prompt_and_id() {
        let event = event(
            "task_awaiting_input",
            awaiting_payload(Some(serde_json::json!({
                "kind": "permission",
                "message": "Allow `rm -rf /`?",
            }))),
        );
        let alert = alert_for(&event).expect("alert");
        assert_eq!(alert.title, "implement_issue needs input");
        assert_eq!(alert.body, "Allow `rm -rf /`?");
        assert_eq!(alert.task_id, "task-123");
    }

    #[test]
    fn alert_for_tolerates_a_missing_awaiting_reason() {
        let event = event("task_awaiting_input", awaiting_payload(None));
        let alert = alert_for(&event).expect("alert");
        assert_eq!(alert.body, "needs your input");
    }

    #[test]
    fn alert_for_requires_a_task_id() {
        let event = event("task_awaiting_input", serde_json::json!({ "name": "x" }));
        assert!(alert_for(&event).is_none());
    }

    #[test]
    fn alert_for_builds_a_finished_alert_from_the_summary() {
        let event = event(
            "task_finished",
            finished_payload(true, Some("Opened PR #42")),
        );
        let alert = alert_for(&event).expect("alert");
        assert_eq!(alert.title, "implement_issue finished");
        assert_eq!(alert.body, "Opened PR #42");
        assert_eq!(alert.task_id, "task-123");
    }

    #[test]
    fn alert_for_marks_a_failed_finish() {
        let event = event(
            "task_finished",
            finished_payload(false, Some("tests failed")),
        );
        let alert = alert_for(&event).expect("alert");
        assert_eq!(alert.title, "implement_issue failed");
        assert_eq!(alert.body, "tests failed");
    }

    #[test]
    fn alert_for_falls_back_when_the_summary_is_missing_or_blank() {
        let no_summary = event("task_finished", finished_payload(true, None));
        assert_eq!(
            alert_for(&no_summary).expect("alert").body,
            "finished successfully"
        );
        let blank = event("task_finished", finished_payload(false, Some("   ")));
        assert_eq!(
            alert_for(&blank).expect("alert").body,
            "finished with an error"
        );
    }

    #[test]
    fn alert_for_ignores_terminal_duplicates_and_other_kinds() {
        // Only `task_finished` notifies for a terminal run; `task_completed` /
        // `task_failed` must not double up.
        for kind in [
            "task_completed",
            "task_failed",
            "task_started",
            "issue_created",
        ] {
            let event = event(kind, finished_payload(true, Some("done")));
            assert!(alert_for(&event).is_none(), "{kind} should not alert");
        }
    }

    #[test]
    fn notifiable_kinds_have_labels_and_cover_the_alert_builder() {
        for kind in NOTIFIABLE_KINDS {
            assert_ne!(event_label(kind), kind.as_str(), "{kind:?}");
            let payload = if *kind == EventKind::TaskAwaitingInput {
                awaiting_payload(Some(serde_json::json!({ "message": "hi" })))
            } else {
                finished_payload(true, Some("done"))
            };
            let event = event(kind.as_str(), payload);
            assert!(alert_for(&event).is_some(), "{kind:?} should alert");
        }
    }

    #[test]
    fn subscription_defaults_to_awaiting_input() {
        let subscription = Subscription::default();
        assert!(subscription.contains(&EventKind::TaskAwaitingInput));
        assert!(!subscription.contains(&EventKind::TaskFinished));
        assert!(!subscription.is_empty());
        assert_eq!(subscription.as_str(), "task_awaiting_input");
    }

    #[test]
    fn subscription_set_and_as_str_are_canonical_and_ordered() {
        let mut subscription = Subscription::default();
        subscription.set(&EventKind::TaskFinished, true);
        assert_eq!(subscription.as_str(), "task_awaiting_input,task_finished");
        // The canonical order is NOTIFIABLE_KINDS order, not insertion order.
        let mut other = Subscription {
            kinds: vec![EventKind::TaskFinished, EventKind::TaskAwaitingInput],
        };
        assert_eq!(other.as_str(), "task_awaiting_input,task_finished");
        assert_eq!(other.kinds().len(), 2);

        other.set(&EventKind::TaskAwaitingInput, false);
        assert_eq!(other.as_str(), "task_finished");
        other.set(&EventKind::TaskFinished, false);
        assert!(other.is_empty());
        assert_eq!(other.as_str(), "none");
    }

    #[test]
    fn subscription_ignores_unknown_names_and_kinds() {
        let mut subscription = Subscription::default();
        // Not a notifiable kind: ignored.
        subscription.set(&EventKind::TaskStarted, true);
        assert!(!subscription.contains(&EventKind::TaskStarted));
        assert_eq!(
            Subscription::parse("task_finished,bogus,task_started,"),
            Subscription::parse("task_finished")
        );
    }

    #[test]
    fn subscription_round_trips_through_its_token() {
        for kind in NOTIFIABLE_KINDS {
            let mut subscription = Subscription::default();
            subscription.set(kind, true);
            assert_eq!(Subscription::parse(&subscription.as_str()), subscription);
        }
        // The empty set persists as `none` and parses back to empty.
        let empty = Subscription { kinds: Vec::new() };
        assert_eq!(empty.as_str(), "none");
        assert!(Subscription::parse("none").is_empty());
        assert!(Subscription::parse("").is_empty());
    }

    #[test]
    fn deep_link_is_the_inbox_task_route() {
        assert_eq!(deep_link("abc"), "/inbox/abc");
    }

    #[test]
    fn should_notify_requires_granted_and_visible() {
        assert!(should_notify(Permission::Granted, false));
        assert!(!should_notify(Permission::Granted, true));
        assert!(!should_notify(Permission::Denied, false));
        assert!(!should_notify(Permission::NotRequested, false));
    }

    #[test]
    fn permission_labels_and_predicates() {
        assert_eq!(Permission::NotRequested.label(), "not requested");
        assert_eq!(Permission::Granted.label(), "granted");
        assert_eq!(Permission::Denied.label(), "denied");

        assert!(Permission::Granted.is_granted());
        assert!(!Permission::Denied.is_granted());
        assert!(!Permission::NotRequested.is_granted());

        assert!(Permission::NotRequested.needs_request());
        assert!(!Permission::Granted.needs_request());
        assert!(!Permission::Denied.needs_request());

        assert_eq!(Permission::default(), Permission::NotRequested);
    }

    #[test]
    fn permission_str_round_trips() {
        for permission in [
            Permission::NotRequested,
            Permission::Granted,
            Permission::Denied,
        ] {
            assert_eq!(Permission::parse(permission.as_str()), permission);
        }
        assert_eq!(Permission::parse("bogus"), Permission::NotRequested);
        assert_eq!(Permission::parse(""), Permission::NotRequested);
    }

    #[test]
    fn notification_id_is_deterministic_and_distinct() {
        assert_eq!(notification_id("task-123"), notification_id("task-123"));
        assert_ne!(notification_id("task-123"), notification_id("task-456"));
        // Android ids must be non-negative and fit a Java int.
        assert!(notification_id("task-123") >= 0);
        assert!(notification_id("") >= 0);
    }

    #[test]
    fn notify_is_a_noop_on_host() {
        notify("title", "body", "task-123");
    }
}
