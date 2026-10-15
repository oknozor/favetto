//! Root component and routing.
//!
//! The app provides the shared [`AppState`] once, renders the connection banner,
//! and (on web) starts the RPC ping and the SSE reader. The authenticated routes
//! share an [`AppLayout`] that renders the responsive navigation. The Inbox is
//! the default landing view; Tasks, Catalog and Workflow plug into the same
//! route enum.

use dioxus::prelude::*;
use uuid::Uuid;

use crate::components::nav::AppNav;
use crate::state::{AppState, ConnectionState};
use crate::views::catalog::{Catalog, CatalogDetail};
use crate::views::inbox::Inbox;
use crate::views::pair::Pair;
use crate::views::schedules::Schedules;
use crate::views::settings::Settings;
use crate::views::tasks::{TaskDetail, TasksList};
use crate::views::workflow::{WorkflowDetail, WorkflowList};

/// Deep-linkable client routes.
///
/// Deep links matter because `ntfy` pages link straight to work (for example
/// `/inbox/:task_id`); keeping routes in one enum means the daemon and the
/// Android native app share the same navigation vocabulary.
#[derive(Routable, Clone, PartialEq, Debug)]
pub enum Route {
    /// Onboarding: configure the daemon (Android) and exchange a pairing code.
    /// Unauthenticated, so it stays outside the [`AppLayout`].
    #[route("/pair")]
    Pair {},
    /// The authenticated area, wrapped in the shared navigation shell.
    #[layout(AppLayout)]
    /// The default landing view: the Inbox. The variant is still named `Home`
    /// so `views::pair` can keep navigating to `Route::Home {}`.
    #[route("/", Inbox)]
    Home {},
    /// Deep-linkable alias for the Inbox (`ntfy` links use `/inbox/:task_id`).
    #[route("/inbox")]
    Inbox {},
    /// Deep link for notification clicks: `/inbox/:task_id`.
    #[route("/inbox/:task_id")]
    InboxTask { task_id: String },
    /// The task list, filterable by status.
    #[route("/tasks")]
    TasksList {},
    /// A single task with its run history, failure detail and output.
    #[route("/tasks/:id")]
    TaskDetail { id: String },
    /// The searchable list of catalog definitions.
    #[route("/catalog")]
    Catalog {},
    /// One catalog definition, rendered as Markdown.
    ///
    /// The name is a catch-all (`:..name`) because repo-qualified task
    /// identities carry a `/` (`repo:<repo>/<task>`): the link splits the name
    /// into segments and the view rejoins them. A single `String` segment would
    /// leave the tail as a trailing segment the router cannot match.
    #[route("/catalog/:..name")]
    CatalogDetail { name: Vec<String> },
    /// The runtime workflow board plus the catalog graph.
    #[route("/workflow")]
    WorkflowList {},
    /// The `workflow.inspect` tree for one workflow root.
    #[route("/workflow/:root_id")]
    WorkflowDetail { root_id: Uuid },
    /// The cron schedule manager.
    #[route("/schedules")]
    Schedules {},
    /// Notification channels, sent history, hooks and session info.
    #[route("/settings")]
    Settings {},
}

/// The shared shell for the authenticated routes: navigation + outlet.
///
/// The skip link is the first focusable element so keyboard users can jump
/// straight to the view (each view's `<main id="favetto-app">`).
#[component]
fn AppLayout() -> Element {
    rsx! {
        a { class: "skip-link", href: "#favetto-app", "Skip to content" }
        AppNav {}
        div { class: "app-content", Outlet::<Route> {} }
    }
}

/// The `/inbox/:task_id` deep link from a notification click.
///
/// The canonical task page stays at `/tasks/:id`; this lands on the Inbox,
/// mirroring the `ntfy` deep link. The `task_id` is kept so a later enhancement
/// can focus the awaiting card.
#[component]
fn InboxTask(task_id: String) -> Element {
    let _ = task_id;
    rsx! {
        Inbox {}
    }
}

/// The application root: stylesheet, layout shell, and router outlet.
#[component]
pub fn App() -> Element {
    let mut state = use_context_provider(AppState::new);

    // Capture `?pair=CODE` once, before `Home`'s guard navigates to `/pair` and
    // drops the query string. The pairing form consumes it.
    use_effect(move || {
        if (state.pending_pair_code)().is_none() {
            if let Some(code) = crate::views::pair::pair_code_from_url() {
                state.pending_pair_code.set(Some(code));
            }
        }
    });

    // Mirror the browser's current permission into state once the app mounts.
    use_effect(move || {
        let current = crate::notifications::current_permission();
        if (state.notification_permission)() != current {
            state.notification_permission.set(current);
        }
    });

    // On Android the OS posts notifications; a tap carries the task id as an
    // intent extra. Poll for it and open the deep link. On web the notification
    // click handler navigates directly, so this is Android-only.
    #[cfg(target_os = "android")]
    {
        let nav = use_navigator();
        use_future(move || async move {
            loop {
                if let Some(task_id) = crate::notifications::take_pending_deep_link() {
                    nav.push(Route::InboxTask { task_id });
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        });
    }

    let base = (state.daemon_url)();
    let token = (state.token)();

    rsx! {
        document::Link { rel: "stylesheet", href: asset!("/assets/tailwind.css") }
        ConnectionBanner {}
        // No token means no transport, so nothing 401-loops before pairing.
        // Remounting on a credential change restarts the connection.
        if let Some(token) = token {
            Transport { key: "{token}|{base}", base, token }
        }
        Router::<Route> {}
    }
}

/// Starts the RPC/SSE transport. Keyed by the credentials in [`App`] so a fresh
/// pairing (or a daemon-URL change) tears down the old stream and reconnects.
#[component]
fn Transport(base: String, token: String) -> Element {
    let state = use_context::<AppState>();
    use_future(move || {
        let (base, token) = (base.clone(), Some(token.clone()));
        async move { connect(state, base, token).await }
    });
    rsx! {}
}

/// A banner shown whenever the client is not connected, or is not paired yet.
///
/// A healthy connection is deliberately quiet — a small status pill rather than
/// a full-width bar — so it does not dominate the chrome. Disconnecting is an
/// explicit action on the Settings page, not a button on every screen.
#[component]
fn ConnectionBanner() -> Element {
    let state = use_context::<AppState>();
    if (state.token)().is_none() {
        return rsx! {
            div { class: "connection-banner disconnected", role: "status",
                "Not signed in — sign in with your username and password (or a pairing code) to connect."
            }
        };
    }
    rsx! {
        match (state.connection)() {
            ConnectionState::Connected => rsx! {
                div { class: "connection-status connected", role: "status",
                    span { class: "connection-dot", aria_hidden: "true" }
                    "Connected"
                }
            },
            ConnectionState::Reconnecting => rsx! {
                div { class: "connection-banner reconnecting", role: "status",
                    "Reconnecting to the daemon…"
                }
            },
            ConnectionState::Disconnected => rsx! {
                div { class: "connection-banner disconnected", role: "status",
                    "Disconnected — check the token and daemon URL."
                }
            },
        }
    }
}

/// Ping once to establish the connection, then stream `/events` forever, feeding
/// pushes into the cache and connection state. Runs on every target: the browser
/// uses `fetch`, native builds use `reqwest` (see `crate::http`).
async fn connect(state: AppState, base: String, token: Option<String>) {
    use crate::config;
    use crate::sse::{self, SseStatus};

    let url = config::events_url_for(&base);
    let cursor = (state.cache)().cursor;

    // The ping runs alongside the stream so the banner flips to connected as
    // soon as either succeeds.
    spawn(ping_once(state, base, token.clone()));

    sse::run(
        url,
        token,
        cursor,
        move |event| apply_event(state, event),
        move |status| {
            let mut state = state;
            let next = match status {
                SseStatus::Connected => ConnectionState::Connected,
                SseStatus::Reconnecting { .. } => ConnectionState::Reconnecting,
                SseStatus::Unauthorized => {
                    // A rotated/revoked token: forget it; the guard sends us
                    // back to `/pair`.
                    state.clear_token();
                    ConnectionState::Disconnected
                }
            };
            state.connection.set(next);
        },
    )
    .await;
}

/// One `system.ping` to set the initial connection state.
async fn ping_once(mut state: AppState, base: String, token: Option<String>) {
    use crate::client::{ClientError, RpcClient};
    use favetto_wire::rpc::PingParams;

    let Some(token) = token else {
        state.connection.set(ConnectionState::Disconnected);
        return;
    };
    let client = RpcClient::new(base).with_token(Some(token));
    let connection = match client.ping(PingParams {}).await {
        Ok(_) => ConnectionState::Connected,
        Err(ClientError::Unauthorized) => {
            // Rotated/revoked token: drop it; the guard sends us to `/pair`.
            state.clear_token();
            ConnectionState::Disconnected
        }
        Err(_) => ConnectionState::Disconnected,
    };
    state.connection.set(connection);
}

/// Feed one dispatched SSE frame into the cursor and the caches.
fn apply_event(mut state: AppState, event: crate::sse::SseEvent) {
    use crate::sse::SseEvent;

    if let Some(cursor) = event.cursor() {
        state.set_cursor(cursor);
    }
    let SseEvent { event, data, .. } = event;
    let method = event.as_deref().unwrap_or(favetto_wire::rpc::push::EVENT);
    if crate::state::is_catalog_push(method) {
        state.bump_catalog();
    }
    if let Ok(params) = serde_json::from_str::<serde_json::Value>(&data) {
        if method == favetto_wire::rpc::push::EVENT {
            if let Ok(event) = serde_json::from_value::<favetto_wire::model::Event>(params.clone())
            {
                handle_awaiting(state, &event);
            }
        }
        state.cache.write().apply_push(method, &params);
    }
}

/// Raise a notification for a `task_awaiting_input` event.
///
/// The first awaiting event asks for permission (fire-and-forget); every later
/// event notifies when permission is granted and the app is visible.
fn handle_awaiting(mut state: AppState, event: &favetto_wire::model::Event) {
    use crate::notifications;

    let Some(alert) = notifications::awaiting_alert(event) else {
        return;
    };
    let permission = (state.notification_permission)();
    if permission.needs_request() {
        spawn(async move {
            let next = notifications::request_permission().await;
            state.notification_permission.set(next);
            if notifications::should_notify(next, notifications::document_hidden()) {
                notifications::notify(&alert.title, &alert.body, &alert.task_id);
            }
        });
    } else if notifications::should_notify(permission, notifications::document_hidden()) {
        notifications::notify(&alert.title, &alert.body, &alert.task_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The root component mounts without panicking on a host build; this guards
    /// the router/stylesheet wiring that a `dx build` would otherwise be the
    /// first to catch.
    #[test]
    fn app_renders() {
        let mut dom = VirtualDom::new(App);
        dom.rebuild_in_place();
    }

    /// The client links `favetto-wire` directly, so it speaks the daemon's
    /// exact status vocabulary with no re-derivation. This also keeps the path
    /// dependency honest for host `cargo test` runs.
    #[test]
    fn shares_the_wire_status_vocabulary() {
        assert_eq!(
            favetto_wire::model::TaskStatus::Running,
            favetto_wire::model::TaskStatus::Running
        );
    }

    /// `/pair` is the onboarding route both targets deep-link to.
    #[test]
    fn pair_route_round_trips() {
        assert_eq!(Route::Pair {}.to_string(), "/pair");
        assert_eq!("/pair".parse::<Route>().unwrap(), Route::Pair {});
    }

    #[test]
    fn catalog_routes_serialize() {
        assert_eq!(Route::Catalog {}.to_string(), "/catalog");
        assert_eq!(
            Route::CatalogDetail {
                name: vec!["foo".into()]
            }
            .to_string(),
            "/catalog/foo"
        );
    }

    /// Repo-qualified task names contain a `/`, so the detail route is a
    /// catch-all: the split segments must round-trip to the original name.
    #[test]
    fn catalog_detail_route_round_trips_repo_qualified_names() {
        let route = Route::CatalogDetail {
            name: vec!["repo:Favetto".into(), "open_github_issue".into()],
        };
        assert_eq!(route.to_string(), "/catalog/repo:Favetto/open_github_issue");
        assert_eq!(route.to_string().parse::<Route>().unwrap(), route);
    }

    #[test]
    fn schedules_and_settings_routes_serialize() {
        assert_eq!(Route::Schedules {}.to_string(), "/schedules");
        assert_eq!(Route::Settings {}.to_string(), "/settings");
        assert_eq!("/schedules".parse::<Route>().unwrap(), Route::Schedules {});
        assert_eq!("/settings".parse::<Route>().unwrap(), Route::Settings {});
    }

    /// The Inbox is reachable both at the root (the `Home` variant) and at the
    /// deep-linkable `/inbox` alias.
    #[test]
    fn inbox_routes_serialize() {
        assert_eq!(Route::Home {}.to_string(), "/");
        assert_eq!(Route::Inbox {}.to_string(), "/inbox");
        assert_eq!("/".parse::<Route>().unwrap(), Route::Home {});
        assert_eq!("/inbox".parse::<Route>().unwrap(), Route::Inbox {});
    }

    /// A notification click deep-links to `/inbox/:task_id`.
    #[test]
    fn inbox_task_route_round_trips() {
        assert_eq!(
            Route::InboxTask {
                task_id: "abc".into()
            }
            .to_string(),
            "/inbox/abc"
        );
        assert_eq!(
            "/inbox/abc".parse::<Route>().unwrap(),
            Route::InboxTask {
                task_id: "abc".into()
            }
        );
    }
}
