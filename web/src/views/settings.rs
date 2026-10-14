//! Settings view (`/settings`): notification channels, sent history, hooks and
//! session information.
//!
//! The daemon keeps *channel configuration* file-based (see
//! `docs/design/web-client.md` §9), so this view does not edit it: it lists the
//! supported channels, sends a `notifications.test` through the selected one,
//! shows the persisted `notifications.list` sent history, and adds a
//! `hooks.upsert` hook that reacts to an event kind. The session card shows the
//! daemon `cwd` from `system.ping` and a "Disconnect" action that drops the
//! bearer token and returns to `/pair`.

use dioxus::prelude::*;
use favetto_wire::model::{EventKind, NotificationRecord};
use favetto_wire::rpc::{
    HookUpsertParams, NotificationTestParams, NotificationsListParams, PingParams,
};

use crate::app::Route;
use crate::client::RpcClient;
use crate::state::AppState;

/// The notification channels the daemon implements (`crates/favetto/src/notify.rs`).
pub const CHANNELS: [&str; 3] = ["log", "webhook", "ntfy"];

/// A human label for a channel.
pub fn channel_label(channel: &str) -> &'static str {
    match channel {
        "log" => "Log",
        "webhook" => "Webhook",
        "ntfy" => "ntfy",
        _ => "Unknown",
    }
}

/// The target input's label for a channel, or `None` when it needs no target
/// (`log`).
pub fn channel_target_label(channel: &str) -> Option<&'static str> {
    match channel {
        "webhook" => Some("Webhook URL"),
        "ntfy" => Some("Topic"),
        _ => None,
    }
}

/// Validate an `http(s)://` URL and return it trimmed.
pub fn validate_http_url(input: &str) -> Result<String, String> {
    let url = input.trim();
    if url.is_empty() {
        return Err("URL is required".into());
    }
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or_else(|| "URL must start with http:// or https://".to_string())?;
    if rest.is_empty() || rest.contains(char::is_whitespace) {
        return Err("Enter a valid URL".into());
    }
    Ok(url.to_string())
}

/// Build the `config` object `notifications.test`/`hooks.upsert` pass to the
/// channel. `log` needs none; `webhook` needs a validated URL; `ntfy` a topic.
pub fn build_channel_config(channel: &str, target: &str) -> Result<serde_json::Value, String> {
    match channel {
        "log" => Ok(serde_json::Value::Null),
        "webhook" => {
            let url = validate_http_url(target)?;
            Ok(serde_json::json!({ "url": url }))
        }
        "ntfy" => {
            let topic = target.trim();
            if topic.is_empty() {
                Err("topic is required".into())
            } else if topic.contains(char::is_whitespace) {
                Err("topic must not contain spaces".into())
            } else {
                Ok(serde_json::json!({ "topic": topic }))
            }
        }
        other => Err(format!("unknown channel '{other}'")),
    }
}

/// Validate the "Send test" form into `notifications.test` params.
pub fn build_test_params(
    channel: &str,
    target: &str,
    subject: &str,
    body: &str,
) -> Result<NotificationTestParams, String> {
    let config = build_channel_config(channel, target)?;
    let subject = subject.trim();
    let body = body.trim();
    Ok(NotificationTestParams {
        channel: channel.to_string(),
        config: Some(config),
        subject: (!subject.is_empty()).then(|| subject.to_string()),
        body: (!body.is_empty()).then(|| body.to_string()),
    })
}

/// Validate the hook form into `hooks.upsert` params.
///
/// The event accepts either the wire `snake_case` name or the spec's
/// `PascalCase` spelling and is sent in its canonical `snake_case` form.
pub fn build_hook_params(
    event: &str,
    channel: &str,
    target: &str,
) -> Result<HookUpsertParams, String> {
    let event = event.trim();
    let kind =
        EventKind::from_name(event).ok_or_else(|| format!("unknown event kind '{event}'"))?;
    let config = build_channel_config(channel, target)?;
    Ok(HookUpsertParams {
        event: kind.as_str().to_string(),
        channel: channel.to_string(),
        config: Some(config),
    })
}

/// A bearer client built from the shared state.
fn client(state: AppState) -> RpcClient {
    RpcClient::new((state.daemon_url)()).with_token((state.token)())
}

/// The `/settings` view.
#[component]
pub fn Settings() -> Element {
    rsx! {
        main { id: "favetto-app", class: "settings-view",
            h1 { "Settings" }
            SessionSection {}
            AppNotificationsSection {}
            NotificationsSection {}
            HooksSection {}
        }
    }
}

/// Session information (`system.ping`) and the "Disconnect" action.
#[component]
fn SessionSection() -> Element {
    let state = use_context::<AppState>();
    let nav = use_navigator();
    let ping = use_resource(move || {
        let base = (state.daemon_url)();
        let token = (state.token)();
        async move {
            RpcClient::new(base)
                .with_token(token)
                .ping(PingParams {})
                .await
                .map_err(|e| e.to_string())
        }
    });

    let daemon = {
        let url = (state.daemon_url)();
        if url.is_empty() {
            "same origin".to_string()
        } else {
            url
        }
    };
    let token_status = if (state.token)().is_some() {
        "paired"
    } else {
        "not paired"
    };
    let cwd = match ping() {
        Some(Ok(result)) => result.cwd,
        Some(Err(err)) => format!("unavailable ({err})"),
        None => "loading…".to_string(),
    };

    rsx! {
        section { class: "settings-section",
            h2 { "Session" }
            dl { class: "settings-facts",
                dt { "Daemon" }
                dd { "{daemon}" }
                dt { "Working directory" }
                dd { code { "{cwd}" } }
                dt { "Token" }
                dd { "{token_status}" }
            }
            button {
                class: "btn danger",
                onclick: move |_| {
                    let mut state = state;
                    state.clear_token();
                    nav.replace(Route::Pair {});
                },
                "Disconnect"
            }
        }
    }
}

/// Browser/WebView notification permission and a test. Daemon channels (`ntfy`,
/// webhook) are managed by [`NotificationsSection`].
#[component]
fn AppNotificationsSection() -> Element {
    let mut state = use_context::<AppState>();
    let mut busy = use_signal(|| false);
    let permission = (state.notification_permission)();

    let enable = move |_| {
        busy.set(true);
        spawn(async move {
            let next = crate::notifications::request_permission().await;
            state.notification_permission.set(next);
            busy.set(false);
        });
    };
    let test = move |_| {
        crate::notifications::notify("favetto", "Test notification from favetto", "test");
    };

    rsx! {
        section { class: "settings-section",
            h2 { "App notifications" }
            p { class: "settings-hint",
                "Show a system notification when a task needs your input. Background paging to a closed app is handled by the daemon's ntfy channel."
            }
            dl { class: "settings-facts",
                dt { "Permission" }
                dd { span { class: "status-badge", "{permission.label()}" } }
            }
            div { class: "channel-actions",
                button {
                    class: "btn primary",
                    disabled: busy() || permission.is_granted(),
                    onclick: enable,
                    "Enable notifications"
                }
                button {
                    class: "btn",
                    disabled: !permission.is_granted(),
                    onclick: test,
                    "Send test notification"
                }
            }
        }
    }
}

/// Notification channels (each with its own "Send test") and the persisted sent
/// history.
#[component]
fn NotificationsSection() -> Element {
    let state = use_context::<AppState>();
    let mut reload = use_signal(|| 0u64);

    let history = use_resource(move || {
        let base = (state.daemon_url)();
        let token = (state.token)();
        let _ = reload();
        async move {
            RpcClient::new(base)
                .with_token(token)
                .notifications_list(NotificationsListParams { limit: Some(50) })
                .await
                .map_err(|e| e.to_string())
        }
    });

    let rows: Vec<NotificationRecord> = match history() {
        Some(Ok(list)) => list,
        _ => Vec::new(),
    };

    rsx! {
        section { class: "settings-section",
            h2 { "Notifications" }
            p { class: "settings-hint",
                "Channels are configured in the daemon config file; send a test to verify one."
            }
            ul { class: "channel-list",
                for channel in CHANNELS {
                    ChannelRow {
                        channel,
                        on_sent: move |_| reload.set(reload() + 1),
                    }
                }
            }

            h3 { "Sent history" }
            match history() {
                None => rsx! { p { class: "loading", "Loading history…" } },
                Some(Err(err)) => rsx! {
                    div { class: "error-panel", "Could not load history: {err}" }
                },
                Some(Ok(_)) => rsx! {
                    if rows.is_empty() {
                        p { class: "empty-state", "No notifications sent yet." }
                    } else {
                        table { class: "settings-table",
                            thead {
                                tr {
                                    th { "Sent" }
                                    th { "Channel" }
                                    th { "Subject" }
                                    th { "Status" }
                                }
                            }
                            tbody {
                                for record in rows {
                                    tr { key: "{record.id}",
                                        td { "{record.sent_at}" }
                                        td { "{record.channel}" }
                                        td { "{record.subject}" }
                                        td {
                                            span {
                                                class: if record.status == "ok" { "status-badge status-succeeded" } else { "status-badge status-failed" },
                                                "{record.status}"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
            }
        }
    }
}

/// One notification channel with its own target input and "Send test" button.
#[component]
fn ChannelRow(channel: &'static str, on_sent: EventHandler<()>) -> Element {
    let state = use_context::<AppState>();
    let mut target = use_signal(String::new);
    let mut status = use_signal(|| Option::<(bool, String)>::None);
    let mut busy = use_signal(|| false);

    let send = move |_| {
        let params = match build_test_params(channel, &target(), "", "") {
            Ok(params) => params,
            Err(err) => {
                status.set(Some((false, err)));
                return;
            }
        };
        status.set(None);
        busy.set(true);
        spawn(async move {
            match client(state).notifications_test(params).await {
                Ok(result) => {
                    busy.set(false);
                    let message = if result.sent {
                        "sent".to_string()
                    } else {
                        "the daemon did not send the test".to_string()
                    };
                    status.set(Some((result.sent, message)));
                    on_sent.call(());
                }
                Err(err) => {
                    busy.set(false);
                    status.set(Some((false, err.to_string())));
                }
            }
        });
    };

    let target_label = channel_target_label(channel);
    rsx! {
        li { class: "channel-row",
            div { class: "channel-head",
                span { class: "channel-name", "{channel_label(channel)}" }
                code { class: "channel-id", "{channel}" }
            }
            if let Some(label) = target_label {
                label { class: "field",
                    span { class: "field-label", "{label}" }
                    input {
                        class: "input",
                        value: "{target()}",
                        placeholder: if channel == "webhook" { "https://example.com/hook" } else { "my-topic" },
                        oninput: move |event| target.set(event.value()),
                    }
                }
            }
            div { class: "channel-actions",
                button { class: "btn", disabled: busy(), onclick: send, "Send test" }
                if let Some((ok, message)) = status() {
                    span {
                        class: if ok { "settings-success" } else { "spawn-error" },
                        role: "status",
                        "{message}"
                    }
                }
            }
        }
    }
}

/// Add a notification hook (`hooks.upsert`).
#[component]
fn HooksSection() -> Element {
    let state = use_context::<AppState>();
    let mut event_name = use_signal(|| EventKind::TaskFinished.as_str().to_string());
    let mut channel = use_signal(|| CHANNELS[0].to_string());
    let mut target = use_signal(String::new);
    let mut status = use_signal(|| Option::<(bool, String)>::None);
    let mut busy = use_signal(|| false);

    let submit = move |_| {
        let params = match build_hook_params(&event_name(), &channel(), &target()) {
            Ok(params) => params,
            Err(err) => {
                status.set(Some((false, err)));
                return;
            }
        };
        status.set(None);
        busy.set(true);
        spawn(async move {
            match client(state).hooks_upsert(params).await {
                Ok(_) => {
                    busy.set(false);
                    status.set(Some((true, "Hook added.".to_string())));
                }
                Err(err) => {
                    busy.set(false);
                    status.set(Some((false, err.to_string())));
                }
            }
        });
    };

    let current_channel = channel();
    let target_label = channel_target_label(&current_channel);

    rsx! {
        section { class: "settings-section",
            h2 { "Hooks" }
            p { class: "settings-hint",
                "Deliver a notification to a channel whenever an event kind fires. Hooks are in-memory and reset when the daemon restarts."
            }
            div { class: "settings-form",
                label { class: "field",
                    span { class: "field-label", "Event" }
                    select {
                        class: "input",
                        value: "{event_name()}",
                        onchange: move |event| event_name.set(event.value()),
                        for kind in EventKind::ALL {
                            option { value: kind.as_str(), "{kind.as_str()}" }
                        }
                    }
                }
                label { class: "field",
                    span { class: "field-label", "Channel" }
                    select {
                        class: "input",
                        value: "{current_channel}",
                        onchange: move |event| channel.set(event.value()),
                        for option in CHANNELS {
                            option { value: "{option}", "{channel_label(option)}" }
                        }
                    }
                }
                if let Some(label) = target_label {
                    label { class: "field",
                        span { class: "field-label", "{label}" }
                        input {
                            class: "input",
                            value: "{target()}",
                            oninput: move |event| target.set(event.value()),
                        }
                    }
                }
                button { class: "btn primary", disabled: busy(), onclick: submit, "Add hook" }
            }
            if let Some((ok, message)) = status() {
                p {
                    class: if ok { "settings-success" } else { "error-panel" },
                    role: "status",
                    "{message}"
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_http_url_accepts_http_and_https() {
        assert_eq!(
            validate_http_url("  https://example.com/hook ").unwrap(),
            "https://example.com/hook"
        );
        assert_eq!(
            validate_http_url("http://192.168.1.10:8080/hook").unwrap(),
            "http://192.168.1.10:8080/hook"
        );
    }

    #[test]
    fn validate_http_url_rejects_bad_input() {
        for input in ["", "   ", "example.com", "ftp://x", "http://", "http://a b"] {
            assert!(validate_http_url(input).is_err(), "expected {input:?}");
        }
    }

    #[test]
    fn channel_config_needs_a_target_where_relevant() {
        assert_eq!(
            build_channel_config("log", "").unwrap(),
            serde_json::Value::Null
        );
        assert_eq!(
            build_channel_config("webhook", "https://example.com/hook").unwrap(),
            serde_json::json!({ "url": "https://example.com/hook" })
        );
        assert_eq!(
            build_channel_config("ntfy", "my-topic").unwrap(),
            serde_json::json!({ "topic": "my-topic" })
        );
        assert!(build_channel_config("webhook", "nope").is_err());
        assert!(build_channel_config("ntfy", " ").is_err());
        assert!(build_channel_config("ntfy", "a b").is_err());
        assert!(build_channel_config("email", "x").is_err());
    }

    #[test]
    fn build_test_params_fills_channel_and_config() {
        let params = build_test_params("webhook", "https://example.com/hook", " hi ", "").unwrap();
        assert_eq!(params.channel, "webhook");
        assert_eq!(
            params.config,
            Some(serde_json::json!({ "url": "https://example.com/hook" }))
        );
        assert_eq!(params.subject.as_deref(), Some("hi"));
        assert_eq!(params.body, None);
    }

    #[test]
    fn build_hook_params_normalizes_the_event_name() {
        let params = build_hook_params("TaskFinished", "log", "").unwrap();
        assert_eq!(params.event, "task_finished");
        assert_eq!(params.channel, "log");
        assert_eq!(params.config, Some(serde_json::Value::Null));

        let params = build_hook_params("task_finished", "log", "").unwrap();
        assert_eq!(params.event, "task_finished");
    }

    #[test]
    fn build_hook_params_rejects_unknown_event() {
        assert!(build_hook_params("nope", "log", "").is_err());
        assert!(build_hook_params("", "log", "").is_err());
    }

    #[test]
    fn channel_labels_and_targets_cover_every_channel() {
        for channel in CHANNELS {
            assert_ne!(channel_label(channel), "Unknown", "{channel}");
        }
        assert_eq!(channel_target_label("log"), None);
        assert_eq!(channel_target_label("webhook"), Some("Webhook URL"));
        assert_eq!(channel_target_label("ntfy"), Some("Topic"));
    }

    fn with_state(view: fn() -> Element) -> Element {
        use_context_provider(AppState::new);
        view()
    }

    #[test]
    fn settings_view_mounts_with_state() {
        let mut dom = VirtualDom::new(|| with_state(|| rsx! { Settings {} }));
        dom.rebuild_in_place();
    }

    #[test]
    fn app_notifications_section_mounts_with_state() {
        let mut dom = VirtualDom::new(|| with_state(|| rsx! { AppNotificationsSection {} }));
        dom.rebuild_in_place();
    }
}
