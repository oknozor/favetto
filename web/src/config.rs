//! Platform-aware daemon URL configuration.
//!
//! The client speaks to exactly one daemon, addressed by a *base URL* that is
//! deliberately allowed to be empty:
//!
//! - **Web** (served by the daemon): the base is empty and every call is a
//!   same-origin relative URL (`/rpc`, `/events`, `/pair/exchange`). This keeps
//!   the SPA free of CORS and of any hard-coded host.
//! - **Android** (native, connects to a remote daemon): the base is a
//!   user-configured absolute URL such as `http://192.168.1.10:8080` or
//!   `https://favetto.example.com`; calls are prefixed with it.
//!
//! Keeping the join in one place means the RPC client, the SSE reader and the
//! pairing screen can never disagree about where the daemon lives.

use crate::state;

/// Well-known daemon paths, appended to the configured base URL.
pub const RPC_PATH: &str = "/rpc";
/// The SSE push channel.
pub const EVENTS_PATH: &str = "/events";
/// The pairing token exchange.
pub const PAIR_EXCHANGE_PATH: &str = "/pair/exchange";

/// The default daemon URL for the current platform.
///
/// Both the web and Android builds default to the empty string: web means
/// "same origin", and Android means "not configured yet" (the user is prompted
/// to enter one before pairing).
pub fn default_daemon_url() -> String {
    String::new()
}

/// Normalize a user-entered base URL: trim surrounding whitespace and any
/// trailing slashes so joining never produces `//rpc`.
pub fn normalize_base(base: &str) -> String {
    base.trim().trim_end_matches('/').to_string()
}

/// Join a base URL to a daemon path.
///
/// An empty (or whitespace-only) base yields the bare path, which a browser
/// resolves against the current origin.
pub fn url_for(base: &str, path: &str) -> String {
    let base = normalize_base(base);
    if base.is_empty() {
        path.to_string()
    } else {
        format!("{base}{path}")
    }
}

/// The configured daemon URL, or empty for same-origin (web) / unconfigured
/// (Android).
pub fn daemon_url() -> String {
    state::load_daemon_url()
}

/// `{daemon_url}/rpc` (or the same-origin `/rpc`).
pub fn rpc_url() -> String {
    rpc_url_for(&daemon_url())
}

/// [`rpc_url`] for an explicit base, without reading storage.
pub fn rpc_url_for(base: &str) -> String {
    url_for(base, RPC_PATH)
}

/// `{daemon_url}/events` (or the same-origin `/events`).
pub fn events_url() -> String {
    events_url_for(&daemon_url())
}

/// [`events_url`] for an explicit base, without reading storage.
pub fn events_url_for(base: &str) -> String {
    url_for(base, EVENTS_PATH)
}

/// `{daemon_url}/pair/exchange` (or the same-origin `/pair/exchange`).
pub fn pair_exchange_url() -> String {
    pair_exchange_url_for(&daemon_url())
}

/// [`pair_exchange_url`] for an explicit base, without reading storage.
pub fn pair_exchange_url_for(base: &str) -> String {
    url_for(base, PAIR_EXCHANGE_PATH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_origin_base_yields_relative_paths() {
        for base in ["", "   ", "/"] {
            assert_eq!(rpc_url_for(base), "/rpc");
            assert_eq!(events_url_for(base), "/events");
            assert_eq!(pair_exchange_url_for(base), "/pair/exchange");
        }
    }

    #[test]
    fn configured_base_is_prefixed() {
        let base = "http://192.168.1.10:8080";
        assert_eq!(rpc_url_for(base), "http://192.168.1.10:8080/rpc");
        assert_eq!(events_url_for(base), "http://192.168.1.10:8080/events");
        assert_eq!(
            pair_exchange_url_for(base),
            "http://192.168.1.10:8080/pair/exchange"
        );
    }

    #[test]
    fn trailing_slashes_and_whitespace_are_trimmed() {
        assert_eq!(
            normalize_base("  https://favetto.example.com/  "),
            "https://favetto.example.com"
        );
        assert_eq!(
            rpc_url_for("https://favetto.example.com///"),
            "https://favetto.example.com/rpc"
        );
    }

    #[test]
    fn default_is_same_origin_on_every_platform() {
        assert_eq!(default_daemon_url(), "");
        assert_eq!(url_for(&default_daemon_url(), RPC_PATH), "/rpc");
    }
}
