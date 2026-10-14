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

/// True on platforms where the daemon is not same-origin and the user must
/// enter a URL before pairing (Android). Web is always same-origin, so the URL
/// step never appears there.
pub fn daemon_url_required() -> bool {
    cfg!(target_os = "android")
}

/// Validate and normalize a user-entered daemon base URL.
///
/// Rejects empty input, a missing `http://`/`https://` scheme, and embedded
/// whitespace. On success returns the normalized base (trailing slash trimmed)
/// so it can be stored and joined without producing `//rpc`.
pub fn validate_daemon_url(input: &str) -> Result<String, String> {
    let url = normalize_base(input);
    if url.is_empty() {
        return Err("Enter the daemon address".into());
    }
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or_else(|| "Address must start with http:// or https://".to_string())?;
    if rest.is_empty() || rest.contains(char::is_whitespace) {
        return Err("Enter a valid daemon address".into());
    }
    Ok(url)
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

    #[test]
    fn validate_daemon_url_accepts_http_and_https() {
        assert_eq!(
            validate_daemon_url("http://192.168.1.10:8080"),
            Ok("http://192.168.1.10:8080".into())
        );
        assert_eq!(
            validate_daemon_url("  https://favetto.example.com/  "),
            Ok("https://favetto.example.com".into())
        );
    }

    #[test]
    fn validate_daemon_url_rejects_bad_input() {
        for input in [
            "",
            "   ",
            "192.168.1.10:8080",
            "ftp://x",
            "http://",
            "http://a b",
        ] {
            assert!(
                validate_daemon_url(input).is_err(),
                "expected {input:?} to be rejected"
            );
        }
    }

    #[test]
    fn daemon_url_is_not_required_on_the_host() {
        // The host target is not Android, so the URL step stays hidden.
        assert!(!daemon_url_required());
    }
}
