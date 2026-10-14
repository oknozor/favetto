//! Browser/WebView notifications for commands that need a human.
//!
//! Uses the web `Notification` API, which is available both in a desktop browser
//! and in the Android WebView (vanilla Dioxus mobile packaging, no custom
//! Kotlin). Background paging when the app is closed is handled by the daemon's
//! `ntfy` channel, not here.
//!
//! The permission enum and the event→alert extraction are pure and unit-tested
//! on the host; only the functions below the `wasm32` gate talk to `web-sys`.

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
        title: format!("{name} needs input"),
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

/// Host stub: there is no browser here.
#[cfg(not(target_arch = "wasm32"))]
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

/// Host stub.
#[cfg(not(target_arch = "wasm32"))]
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

/// Host stub.
#[cfg(not(target_arch = "wasm32"))]
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

/// Host stub.
#[cfg(not(target_arch = "wasm32"))]
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
}
