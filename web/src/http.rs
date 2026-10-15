//! Native HTTP client (non-wasm targets: Android and the host test harness).
//!
//! The browser build talks to the daemon with `fetch`; native builds have no
//! `fetch`, so they share one `reqwest` client. TLS uses rustls with the bundled
//! webpki roots, so Android needs no system OpenSSL and a publicly-trusted
//! certificate (e.g. Let's Encrypt) for the daemon's domain just works.
//!
//! Reusing a single client keeps the connection pool — and therefore the
//! long-lived `/events` SSE stream — warm across calls.

use std::sync::OnceLock;

use reqwest::Client;

/// The process-wide HTTP client.
pub fn client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder()
            .user_agent(concat!("favetto-web/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("failed to build the reqwest client")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_is_built_once_and_reused() {
        let first = client() as *const Client;
        let second = client() as *const Client;
        assert_eq!(first, second);
    }
}
