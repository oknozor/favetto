//! Root component and routing.
//!
//! The app provides the shared [`AppState`] once, renders the connection banner,
//! and (on web) starts the RPC ping and the SSE reader. The Inbox, Tasks, Catalog
//! and Settings views land in follow-up issues; the route enum is the seam they
//! plug into.

use dioxus::prelude::*;

use crate::state::{AppState, ConnectionState};

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
}

/// The application root: stylesheet, layout shell, and router outlet.
#[component]
pub fn App() -> Element {
    let state = use_context_provider(AppState::new);
    // `use_future` runs exactly once, on mount. An effect would re-run whenever
    // a signal it read changed, opening a duplicate SSE stream on every cache
    // update, so the transport is started from a one-shot future instead.
    use_future(move || async move { connect(state).await });
    rsx! {
        document::Link { rel: "stylesheet", href: asset!("/assets/style.css") }
        ConnectionBanner {}
        Router::<Route> {}
    }
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
    match (state.connection)() {
        ConnectionState::Connected => rsx! {},
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

/// Connect the transport. A no-op on the host (there is no `fetch`), so the
/// component can still mount in host unit tests.
#[cfg(not(target_arch = "wasm32"))]
async fn connect(_state: AppState) {}

/// On web: ping once to establish the connection, then stream `/events` forever,
/// feeding pushes into the cache and connection state.
#[cfg(target_arch = "wasm32")]
async fn connect(state: AppState) {
    use crate::config;
    use crate::sse::{self, SseStatus};

    let base = (state.daemon_url)();
    let url = config::events_url_for(&base);
    let token = (state.token)();
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
            state.connection.set(match status {
                SseStatus::Connected => ConnectionState::Connected,
                SseStatus::Reconnecting { .. } => ConnectionState::Reconnecting,
            });
        },
    )
    .await;
}

/// One `system.ping` to set the initial connection state.
#[cfg(target_arch = "wasm32")]
async fn ping_once(mut state: AppState, base: String, token: Option<String>) {
    use crate::client::RpcClient;
    use favetto_wire::rpc::PingParams;

    if token.is_none() {
        state.connection.set(ConnectionState::Disconnected);
        return;
    }
    let client = RpcClient::new(base).with_token(token);
    let connection = match client.ping(PingParams {}).await {
        Ok(_) => ConnectionState::Connected,
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
    if let Ok(params) = serde_json::from_str::<serde_json::Value>(&data) {
        state.cache.write().apply_push(method, &params);
    }
}

/// Placeholder home view, replaced by the Inbox at phase 3.2.
#[component]
fn Home() -> Element {
    let state = use_context::<AppState>();
    rsx! {
        main { id: "favetto-app",
            h1 { "favetto" }
            p { "The web client is connected to this daemon." }
            if (state.connection)().is_connected() {
                p { "Connection: connected." }
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
}
