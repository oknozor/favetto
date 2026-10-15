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

use crate::state::task_display_name;

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

/// A pending-input alert derived from a durable event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    /// Notification title (the task name).
    pub title: String,
    /// Notification body (the agent's prompt).
    pub body: String,
    /// The task to open when the notification is clicked.
    pub task_id: String,
}

/// Build an [`Alert`] from a durable event, or `None` for any other kind.
///
/// `task_awaiting_input` payloads are emitted by
/// `crates/favetto/src/attention.rs::mark_awaiting`: `{ task_id, name, session_id, reason }`
/// with `reason.message` carrying the prompt.
pub fn awaiting_alert(event: &Event) -> Option<Alert> {
    if event.kind != EventKind::TaskAwaitingInput {
        return None;
    }
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
        title: format!("{} needs input", task_display_name(name)),
        body: message.to_string(),
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

/// Android: the decision is persisted by [`request_permission`]; the OS state is
/// authoritative once granted.
#[cfg(target_os = "android")]
pub fn current_permission() -> Permission {
    crate::state::load_permission()
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
    let next = crate::android_notify::request_notification_permission().await;
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

    #[test]
    fn awaiting_alert_extracts_name_prompt_and_id() {
        let event = event(
            "task_awaiting_input",
            awaiting_payload(Some(serde_json::json!({
                "kind": "permission",
                "message": "Allow `rm -rf /`?",
            }))),
        );
        let alert = awaiting_alert(&event).expect("alert");
        assert_eq!(alert.title, "implement_issue needs input");
        assert_eq!(alert.body, "Allow `rm -rf /`?");
        assert_eq!(alert.task_id, "task-123");
    }

    #[test]
    fn awaiting_alert_strips_repo_prefix_from_title() {
        let mut payload = awaiting_payload(None);
        payload["name"] = serde_json::json!("repo:my-repo/triage");
        let event = event("task_awaiting_input", payload);
        let alert = awaiting_alert(&event).expect("alert");
        assert_eq!(alert.title, "triage needs input");
    }

    #[test]
    fn awaiting_alert_ignores_other_kinds() {
        let event = event("task_finished", awaiting_payload(None));
        assert!(awaiting_alert(&event).is_none());
    }

    #[test]
    fn awaiting_alert_tolerates_a_missing_reason() {
        let event = event("task_awaiting_input", awaiting_payload(None));
        let alert = awaiting_alert(&event).expect("alert");
        assert_eq!(alert.body, "needs your input");
    }

    #[test]
    fn awaiting_alert_requires_a_task_id() {
        let event = event("task_awaiting_input", serde_json::json!({ "name": "x" }));
        assert!(awaiting_alert(&event).is_none());
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
