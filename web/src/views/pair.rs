//! Daemon configuration and pairing screen (`/pair`).
//!
//! This is the onboarding flow shared by the two Dioxus targets:
//!
//! - **Web** (served by the daemon): same-origin, so only the pairing-code step
//!   is shown.
//! - **Android**: the daemon is remote, so the user first enters its base URL
//!   (with a `system.ping` "Test Connection" probe), then the pairing code.
//!
//! The code comes from `favetto pair` or a `?pair=CODE` deep link and is
//! exchanged at `POST {base}/pair/exchange` for the same bearer token the TUI
//! uses; [`crate::state::AppState`] persists it in `localStorage`.

use dioxus::prelude::*;
use favetto_wire::rpc::PingParams;

use crate::app::Route;
use crate::client::{self, ClientError, RpcClient};
use crate::config;
use crate::state::{onboarding_step, AppState, OnboardingStep};

/// The `/pair` onboarding screen.
///
/// Shows the daemon-URL step on Android until a URL is configured, the pairing
/// step otherwise, and redirects to `/` once a token is stored.
#[component]
pub fn Pair() -> Element {
    let state = use_context::<AppState>();
    let nav = use_navigator();

    use_effect(move || {
        let step = onboarding_step(
            config::daemon_url_required(),
            &(state.daemon_url)(),
            (state.token)().as_deref(),
        );
        if step == OnboardingStep::Ready {
            nav.replace(Route::Home {});
        }
    });

    let step = onboarding_step(
        config::daemon_url_required(),
        &(state.daemon_url)(),
        (state.token)().as_deref(),
    );
    match step {
        OnboardingStep::DaemonUrl => rsx! { DaemonUrlForm {} },
        _ => rsx! { PairingForm {} },
    }
}

/// Android step 1: enter and validate the daemon base URL.
#[component]
fn DaemonUrlForm() -> Element {
    let mut state = use_context::<AppState>();
    let mut input = use_signal(|| (state.daemon_url)());
    let mut error = use_signal(|| Option::<String>::None);
    let mut probe = use_signal(|| Option::<(bool, String)>::None);
    let busy = use_signal(|| false);

    let submit = move |_| match config::validate_daemon_url(&input()) {
        Ok(url) => {
            state.set_daemon_url(&url);
            error.set(None);
        }
        Err(message) => error.set(Some(message)),
    };
    let test = move |_| match config::validate_daemon_url(&input()) {
        Ok(url) => {
            error.set(None);
            test_connection(url, (state.token)(), probe, busy);
        }
        Err(message) => {
            probe.set(None);
            error.set(Some(message));
        }
    };

    rsx! {
        main { id: "favetto-app", class: "pair-screen",
            h1 { "Connect to a daemon" }
            section { class: "pair-section",
                h2 { "Daemon address" }
                p { class: "pair-hint",
                    "Enter a favetto daemon reachable from this device."
                }
                label { r#for: "daemon-url", "Daemon URL" }
                input {
                    id: "daemon-url",
                    r#type: "url",
                    inputmode: "url",
                    placeholder: "http://192.168.1.10:7878",
                    value: input(),
                    oninput: move |event| input.set(event.value()),
                }
                if let Some(message) = error() {
                    p { class: "pair-error", role: "alert", "{message}" }
                }
                div { class: "pair-actions",
                    button { onclick: submit, "Next" }
                    button { onclick: test, disabled: busy(), "Test Connection" }
                }
                if let Some((ok, message)) = probe() {
                    p {
                        class: if ok { "pair-success" } else { "pair-error" },
                        role: "status",
                        "{message}"
                    }
                }
            }
        }
    }
}

/// Step 2: enter the code from `favetto pair` (or a `?pair=CODE` link).
#[component]
fn PairingForm() -> Element {
    let mut state = use_context::<AppState>();
    let mut code = use_signal(String::new);
    let error = use_signal(|| Option::<String>::None);
    let busy = use_signal(|| false);

    // A `?pair=CODE` deep link pre-fills and submits the code exactly once.
    use_effect(move || {
        if let Some(pending) = (state.pending_pair_code)() {
            state.pending_pair_code.set(None);
            code.set(pending.clone());
            start_pair(state, pending, error, busy);
        }
    });

    let submit = move |_| start_pair(state, code(), error, busy);

    rsx! {
        main { id: "favetto-app", class: "pair-screen",
            h1 { "Pair this device" }
            section { class: "pair-section",
                h2 { "Pairing code" }
                p { class: "pair-hint",
                    "Run `favetto pair` on the daemon host, then enter the code."
                }
                label { r#for: "pair-code", "Pairing code" }
                input {
                    id: "pair-code",
                    r#type: "text",
                    inputmode: "numeric",
                    autocomplete: "one-time-code",
                    value: code(),
                    oninput: move |event| code.set(event.value()),
                }
                if let Some(message) = error() {
                    p { class: "pair-error", role: "alert", "{message}" }
                }
                div { class: "pair-actions",
                    button { onclick: submit, disabled: busy(), "Pair" }
                }
            }
        }
    }
}

/// Exchange `code` for a token and store it. Shared by the submit button and the
/// deep-link auto-submit.
fn start_pair(
    mut state: AppState,
    code: String,
    mut error: Signal<Option<String>>,
    mut busy: Signal<bool>,
) {
    let code = code.trim().to_string();
    if code.is_empty() {
        error.set(Some("Enter the pairing code".into()));
        return;
    }
    busy.set(true);
    error.set(None);
    spawn(async move {
        let base = (state.daemon_url)();
        match client::exchange_pair_code(&base, &code).await {
            Ok(token) => state.set_token(Some(token)),
            Err(err) => error.set(Some(pair_error_message(&err))),
        }
        busy.set(false);
    });
}

/// Probe `system.ping` against a configured URL for the "Test Connection"
/// button. The result is rendered by the caller.
fn test_connection(
    url: String,
    token: Option<String>,
    mut probe: Signal<Option<(bool, String)>>,
    mut busy: Signal<bool>,
) {
    busy.set(true);
    probe.set(None);
    spawn(async move {
        let result = RpcClient::new(&url)
            .with_token(token)
            .ping(PingParams {})
            .await
            .map(|_| ());
        probe.set(Some(connection_message(&result)));
        busy.set(false);
    });
}

/// Read a `?pair=CODE` query parameter from the current URL.
#[cfg(target_arch = "wasm32")]
pub fn pair_code_from_url() -> Option<String> {
    let search = web_sys::window()?.location().search().ok()?;
    let params = web_sys::UrlSearchParams::new_with_str(&search).ok()?;
    params.get("pair").filter(|code| !code.trim().is_empty())
}

/// Host stub: there is no browser URL outside the web/WebView targets.
#[cfg(not(target_arch = "wasm32"))]
pub fn pair_code_from_url() -> Option<String> {
    None
}

/// A user-facing message for a failed pairing exchange.
pub fn pair_error_message(err: &ClientError) -> String {
    match err {
        ClientError::Unauthorized => {
            "That pairing code is invalid or has expired. Run `favetto pair` again for a new code."
                .into()
        }
        ClientError::Network(message) => format!("Could not reach the daemon: {message}"),
        ClientError::Http { status, message } => {
            if message.trim().is_empty() {
                format!("The daemon rejected the request (HTTP {status}).")
            } else {
                format!("The daemon rejected the request (HTTP {status}): {message}")
            }
        }
        other => format!("Pairing failed: {other}"),
    }
}

/// Render a `system.ping` result as `(is_success, message)`.
///
/// `/rpc` is bearer-only, so an unauthenticated probe legitimately gets a 401;
/// that still proves the daemon is reachable, so [`ClientError::Unauthorized`]
/// counts as success here.
pub fn connection_message(result: &Result<(), ClientError>) -> (bool, String) {
    match result {
        Ok(()) => (true, "Connected to the daemon.".into()),
        Err(ClientError::Unauthorized) => {
            (true, "Daemon reachable (authentication pending).".into())
        }
        Err(ClientError::Network(message)) => {
            (false, format!("Could not reach the daemon: {message}"))
        }
        Err(ClientError::Http { status, .. }) => {
            (false, format!("The daemon answered HTTP {status}."))
        }
        Err(other) => (false, format!("Connection failed: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_error_messages_cover_the_common_failures() {
        assert!(pair_error_message(&ClientError::Unauthorized).contains("expired"));
        assert!(pair_error_message(&ClientError::Network("offline".into())).contains("offline"));
        let http = pair_error_message(&ClientError::Http {
            status: 500,
            message: "boom".into(),
        });
        assert!(http.contains("500") && http.contains("boom"));
    }

    #[test]
    fn connection_message_treats_unauthorized_as_reachable() {
        let (ok, message) = connection_message(&Ok(()));
        assert!(ok);
        assert!(message.contains("Connected"));

        let (ok, message) = connection_message(&Err(ClientError::Unauthorized));
        assert!(ok, "a 401 still proves the daemon is up");
        assert!(message.contains("reachable"));

        let (ok, _) = connection_message(&Err(ClientError::Network("offline".into())));
        assert!(!ok);
    }

    #[test]
    fn host_has_no_deep_link_code() {
        assert_eq!(pair_code_from_url(), None);
    }
}
