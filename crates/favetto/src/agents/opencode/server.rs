//! One daemon-owned `opencode serve` singleton.
//!
//! Interactive opencode sessions are moved off the background service and onto a
//! server favetto starts itself: a random loopback port, HTTP Basic auth via
//! `OPENCODE_SERVER_PASSWORD`, and a supervised child that is restarted with
//! exponential backoff. If the server cannot be started (or keeps dying) the
//! endpoint is left empty and every consumer silently falls back to the stdout
//! JSONL parser plus the screen heuristic — the session is never affected.
//!
//! See `docs/design/agent-state-adapters.md` §7.1 and `.favetto/plans/159`.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use serde_json::json;

/// A live managed server: its base URL and the password it requires.
///
/// The password is deliberately not part of `Debug` output — it is only ever
/// handed to children through `OPENCODE_SERVER_PASSWORD`.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Endpoint {
    pub url: String,
    pub password: String,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("url", &self.url)
            .field("password", &"<redacted>")
            .finish()
    }
}

impl Endpoint {
    /// A fresh HTTP client; each request is short and independent.
    pub(crate) fn client(&self) -> reqwest::Client {
        reqwest::Client::new()
    }

    /// A request against `path` on this server, authenticated as `opencode`.
    pub(crate) fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.client()
            .request(method, format!("{}{path}", self.url))
            .basic_auth("opencode", Some(&self.password))
    }
}

/// Process-global managed server. Only ever started outside `cfg(test)`.
struct ManagedServer {
    endpoint: RwLock<Option<Endpoint>>,
    program: Mutex<Option<PathBuf>>,
    /// Serializes concurrent first-starts so only one child is ever spawned.
    starting: tokio::sync::Mutex<()>,
}

static SERVER: OnceLock<ManagedServer> = OnceLock::new();

fn server() -> &'static ManagedServer {
    SERVER.get_or_init(|| ManagedServer {
        endpoint: RwLock::new(None),
        program: Mutex::new(None),
        starting: tokio::sync::Mutex::new(()),
    })
}

#[cfg(test)]
thread_local! {
    /// Test-only override so `command`/`state_source`/`prepare_launch` can
    /// exercise the server path without spawning a process. Thread-local so
    /// parallel tests cannot observe each other's endpoint.
    static TEST_ENDPOINT: std::cell::RefCell<Option<Endpoint>> =
        const { std::cell::RefCell::new(None) };
}

/// The current endpoint, or `None` when the managed server is unavailable.
pub(crate) fn endpoint() -> Option<Endpoint> {
    #[cfg(test)]
    if let Some(ep) = TEST_ENDPOINT.with(|e| e.borrow().clone()) {
        return Some(ep);
    }
    server().endpoint.read().clone()
}

/// Install (or clear) a test endpoint for the current thread. Test-only.
#[cfg(test)]
pub(crate) fn install_endpoint_for_test(ep: Option<Endpoint>) {
    TEST_ENDPOINT.with(|e| *e.borrow_mut() = ep);
}

/// Start the managed server if it is not already running, returning its
/// endpoint. Idempotent and serialized: concurrent callers share one start.
pub(crate) async fn ensure_started(program: &Path) -> Option<Endpoint> {
    if let Some(ep) = endpoint() {
        return Some(ep);
    }
    // Never spawn a real process from the test harness.
    if cfg!(test) {
        return None;
    }
    let _guard = server().starting.lock().await;
    if let Some(ep) = endpoint() {
        return Some(ep);
    }
    match start_once(program).await {
        Ok((ep, child)) => {
            *server().endpoint.write() = Some(ep.clone());
            *server().program.lock() = Some(program.to_path_buf());
            tokio::spawn(supervise(program.to_path_buf(), child));
            Some(ep)
        }
        Err(e) => {
            tracing::warn!(error = %e, "managed opencode server failed to start");
            None
        }
    }
}

/// Spawn one `opencode serve` on a fresh loopback port and wait for health.
async fn start_once(program: &Path) -> anyhow::Result<(Endpoint, Child)> {
    let password = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let port = pick_port()?;
    let mut child = spawn_serve(program, port, &password)?;
    let ep = Endpoint {
        url: format!("http://127.0.0.1:{port}"),
        password,
    };
    if wait_healthy(&ep).await {
        Ok((ep, child))
    } else {
        let _ = child.kill();
        let _ = child.wait();
        anyhow::bail!("opencode serve did not become healthy")
    }
}

/// Watch the managed child and restart it with backoff until the failure budget
/// is exhausted, at which point the endpoint is cleared and consumers fall back.
async fn supervise(program: PathBuf, mut child: Child) {
    const MAX_RESTART_FAILURES: u32 = 5;
    loop {
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => tokio::time::sleep(Duration::from_millis(500)).await,
                Err(_) => break,
            }
        }
        tracing::warn!("managed opencode server exited; restarting");
        let mut failures = 0u32;
        loop {
            if failures >= MAX_RESTART_FAILURES {
                *server().endpoint.write() = None;
                tracing::warn!(
                    "managed opencode server giving up after {MAX_RESTART_FAILURES} failures"
                );
                return;
            }
            tokio::time::sleep(backoff(failures)).await;
            failures += 1;
            match start_once(&program).await {
                Ok((ep, new_child)) => {
                    *server().endpoint.write() = Some(ep);
                    child = new_child;
                    break;
                }
                Err(e) => {
                    tracing::warn!(attempt = failures, error = %e, "managed opencode restart failed");
                }
            }
        }
    }
}

/// Exponential backoff, capped at 5s: 250ms, 500ms, 1s, 2s, 4s, 5s, …
pub(crate) fn backoff(attempt: u32) -> Duration {
    const CAP_MS: u64 = 5_000;
    let ms = 250u64.saturating_mul(1u64 << attempt.min(5));
    Duration::from_millis(ms.min(CAP_MS))
}

/// A free loopback port: bind `:0`, read the assigned port, then release it.
fn pick_port() -> std::io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.local_addr().map(|addr| addr.port())
}

/// Build the `opencode serve` command, preferring the `__agent-exec` supervisor
/// so the server dies with the daemon.
fn spawn_serve(program: &Path, port: u16, password: &str) -> std::io::Result<Child> {
    let mut cmd = if super::super::wrap_agent() {
        let exe = super::super::agent_exec_program().unwrap_or_else(|_| program.to_path_buf());
        let mut c = Command::new(exe);
        c.arg("__agent-exec").arg("--").arg(program);
        c
    } else {
        Command::new(program)
    };
    cmd.arg("serve")
        .arg("--hostname")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string());
    cmd.env("OPENCODE_SERVER_PASSWORD", password);
    cmd.env("TERM", "dumb");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());
    cmd.spawn()
}

/// Poll `/api/info` (the health endpoint) until it answers or we give up.
async fn wait_healthy(ep: &Endpoint) -> bool {
    for _ in 0..80 {
        if let Ok(resp) = ep.request(reqwest::Method::GET, "/api/info").send().await {
            if resp.status().is_success() {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// Create a session at `dir`, optionally with a model, returning its id.
///
/// `model` is `(model_id, provider_id)`.
pub(crate) async fn create_session(
    ep: &Endpoint,
    dir: &Path,
    model: Option<(&str, &str)>,
) -> anyhow::Result<String> {
    let mut body = json!({ "location": { "directory": dir.to_string_lossy() } });
    if let Some((model_id, provider_id)) = model {
        body["model"] = json!({ "id": model_id, "providerID": provider_id });
    }
    let resp = ep
        .request(reqwest::Method::POST, "/api/session")
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    let value: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        anyhow::bail!("opencode session create failed ({status}): {value}");
    }
    value
        .pointer("/data/id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("opencode session create returned no data.id"))
}

/// Seed a session's first turn with `text`.
pub(crate) async fn prompt(ep: &Endpoint, session_id: &str, text: &str) -> anyhow::Result<()> {
    let path = format!("/api/session/{}/prompt", encode_segment(session_id));
    let resp = ep
        .request(reqwest::Method::POST, &path)
        .json(&json!({ "text": text }))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("opencode prompt failed ({})", resp.status());
    }
    Ok(())
}

/// Percent-encode a path segment, leaving only RFC 3986 unreserved bytes.
pub(crate) fn encode_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_port_returns_a_bindable_port() {
        let port = pick_port().unwrap();
        assert_ne!(port, 0);
        // The port was released, so it must be bindable right away.
        assert!(TcpListener::bind(("127.0.0.1", port)).is_ok());
    }

    #[test]
    fn backoff_is_exponential_and_capped() {
        assert_eq!(backoff(0), Duration::from_millis(250));
        assert_eq!(backoff(1), Duration::from_millis(500));
        assert_eq!(backoff(2), Duration::from_millis(1000));
        assert_eq!(backoff(3), Duration::from_millis(2000));
        assert_eq!(backoff(4), Duration::from_millis(4000));
        assert_eq!(backoff(5), Duration::from_millis(5000));
        assert_eq!(backoff(9), Duration::from_millis(5000));
    }

    #[test]
    fn endpoint_request_attaches_basic_auth() {
        use base64::Engine;
        let ep = Endpoint {
            url: "http://127.0.0.1:1".to_string(),
            password: "secret".to_string(),
        };
        let request = ep
            .request(reqwest::Method::GET, "/api/info")
            .build()
            .unwrap();
        let expected = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("opencode:secret")
        );
        assert_eq!(
            request.headers().get("authorization").unwrap(),
            expected.as_str()
        );
        assert_eq!(request.url().as_str(), "http://127.0.0.1:1/api/info");
    }

    #[test]
    fn encode_segment_percent_encodes_reserved_bytes() {
        assert_eq!(encode_segment("ses_abc-1.2~3"), "ses_abc-1.2~3");
        assert_eq!(encode_segment("a/b c"), "a%2Fb%20c");
    }

    #[test]
    fn endpoint_debug_redacts_the_password() {
        let ep = Endpoint {
            url: "http://127.0.0.1:1".to_string(),
            password: "super-secret".to_string(),
        };
        let debug = format!("{ep:?}");
        assert!(
            !debug.contains("super-secret"),
            "debug leaked password: {debug}"
        );
        assert!(debug.contains("<redacted>"));
    }
}
