//! Root component and routing.
//!
//! The app provides the shared [`AppState`] once, renders the connection banner,
//! and (on web) starts the RPC ping and the SSE reader. The Inbox, Tasks, Catalog
//! and Settings views land in follow-up issues; the route enum is the seam they
//! plug into.

use dioxus::prelude::*;
use uuid::Uuid;

use crate::config;
use crate::state::{onboarding_step, AppState, ConnectionState, OnboardingStep};
use crate::views::catalog::{Catalog, CatalogDetail};
use crate::views::pair::Pair;
use crate::views::tasks::{TaskDetail, TasksList};
use crate::views::workflow::{WorkflowDetail, WorkflowList};

/// Deep-linkable client routes.
///
/// Deep links matter because `ntfy` pages link straight to work (for example
/// `/inbox/:task_id`); keeping routes in one enum means the daemon and the
/// Android native app share the same navigation vocabulary.
#[derive(Routable, Clone, PartialEq, Debug)]
pub enum Route {
    /// The default view; the Inbox replaces this in a follow-up issue.
    #[route("/")]
    Home {},
    /// Onboarding: configure the daemon (Android) and exchange a pairing code.
    #[route("/pair")]
    Pair {},
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
    #[route("/catalog/:name")]
    CatalogDetail { name: String },
    /// The runtime workflow board plus the catalog graph.
    #[route("/workflow")]
    WorkflowList {},
    /// The `workflow.inspect` tree for one workflow root.
    #[route("/workflow/:root_id")]
    WorkflowDetail { root_id: Uuid },
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

    let base = (state.daemon_url)();
    let token = (state.token)();

    rsx! {
        document::Link { rel: "stylesheet", href: asset!("/assets/style.css") }
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
#[component]
fn ConnectionBanner() -> Element {
    let state = use_context::<AppState>();
    if (state.token)().is_none() {
        return rsx! {
            div { class: "connection-banner disconnected", role: "status",
                "Not paired — run `favetto pair` and enter the code to connect."
            }
        };
    }
    rsx! {
        match (state.connection)() {
            ConnectionState::Connected => rsx! {
                div { class: "connection-banner connected", role: "status", "Connected." }
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
        div { class: "connection-actions",
            button {
                class: "forget-button",
                onclick: move |_| {
                    let mut state = state;
                    state.forget();
                },
                "Forget daemon"
            }
        }
    }
}

/// Connect the transport. A no-op on the host (there is no `fetch`), so the
/// component can still mount in host unit tests.
#[cfg(not(target_arch = "wasm32"))]
async fn connect(_state: AppState, _base: String, _token: Option<String>) {}

/// On web: ping once to establish the connection, then stream `/events` forever,
/// feeding pushes into the cache and connection state.
#[cfg(target_arch = "wasm32")]
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
#[cfg(target_arch = "wasm32")]
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
#[cfg(target_arch = "wasm32")]
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
        state.cache.write().apply_push(method, &params);
    }
}

/// Placeholder home view, replaced by the Inbox at phase 3.2.
///
/// Guards the authenticated area: until the client is [`OnboardingStep::Ready`]
/// it redirects to `/pair` (daemon-URL step on Android, pairing step otherwise).
#[component]
fn Home() -> Element {
    let state = use_context::<AppState>();
    let nav = use_navigator();
    use_effect(move || {
        let step = onboarding_step(
            config::daemon_url_required(),
            &(state.daemon_url)(),
            (state.token)().as_deref(),
        );
        if step != OnboardingStep::Ready {
            nav.replace(Route::Pair {});
        }
    });
    rsx! {
        main { id: "favetto-app",
            h1 { "favetto" }
            p { "The web client is connected to this daemon." }
            if (state.connection)().is_connected() {
                p { "Connection: connected." }
            }
            nav { class: "home-nav",
                Link { to: Route::TasksList {}, "Tasks" }
                Link { to: Route::WorkflowList {}, "Workflows" }
            }
        }
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
            Route::CatalogDetail { name: "foo".into() }.to_string(),
            "/catalog/foo"
        );
    }
}
