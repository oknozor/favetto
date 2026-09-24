//! Per-launch HTTP hook receiver for external agent CLIs.
//!
//! Some agents (Claude Code) can POST lifecycle events to an HTTP endpoint
//! registered in a per-launch settings file. The daemon exposes one loopback
//! route per launch, keyed by a random token, and routes each payload to the
//! [`StateSource`](crate::agents::state::StateSource) that registered it. This is
//! the server-side half of the claude hook transport; the per-agent payload
//! mapping lives in `agents/claude/hooks.rs`.
//!
//! The receiver is **observe-only**: it never returns a permission decision, so
//! an interactive session's real dialog is still answered by the user in the
//! TUI. When the token is unknown (or hooks are disabled) the endpoint 404s, and
//! a session whose endpoint is silently blocked by `allowedHttpHookUrls` keeps
//! the screen heuristic.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State as AxumState};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use favetto_core::model::AgentStateEvent;

use crate::state::State;

/// Maps one hook payload to the normalized events it produces for one session.
pub(crate) type HookMap = Box<dyn FnMut(&serde_json::Value) -> Vec<AgentStateEvent> + Send>;

/// A resolved loopback hook endpoint plus the settings directory for a launch.
#[derive(Debug, Clone)]
pub struct AgentHookLaunch {
    /// Base URL without the token, e.g.
    /// `http://127.0.0.1:7878/agent-hooks/claude`.
    pub endpoint: String,
    /// Directory the ephemeral per-launch settings files are written to.
    pub dir: PathBuf,
}

/// Args/env an agent wants injected so it starts reporting over hooks.
#[derive(Debug, Clone, Default)]
pub struct HookInjection {
    /// Arguments prepended to the launch (e.g. `["--settings", "<path>"]`).
    pub args: Vec<String>,
    /// Environment variables merged into the launch.
    pub env: std::collections::BTreeMap<String, String>,
}

/// One live per-launch registration.
struct HookEntry {
    tx: mpsc::UnboundedSender<AgentStateEvent>,
    map: HookMap,
}

/// Routes hook payloads to the session that registered the token.
///
/// Cheap to clone (the entry table is shared); [`HookRegistration`] removes a
/// token when the session's state transport stops.
#[derive(Clone, Default)]
pub struct HookRouter {
    entries: Arc<Mutex<HashMap<String, HookEntry>>>,
}

impl fmt::Debug for HookRouter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tokens: Vec<String> = self.entries.lock().keys().cloned().collect();
        f.debug_struct("HookRouter")
            .field("tokens", &tokens)
            .finish()
    }
}

impl HookRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `tx` for a per-launch `token`. The returned guard deregisters on
    /// drop, so the route stops resolving once the session's transport stops.
    pub fn register(
        &self,
        token: String,
        tx: mpsc::UnboundedSender<AgentStateEvent>,
        map: HookMap,
    ) -> HookRegistration {
        self.entries
            .lock()
            .insert(token.clone(), HookEntry { tx, map });
        HookRegistration {
            entries: self.entries.clone(),
            token,
        }
    }

    /// Parse and route one raw hook body to `token`'s session.
    ///
    /// Returns the number of normalized events produced. The body must be valid
    /// JSON; unknown tokens are rejected so a leaked/guessed URL cannot inject
    /// events.
    pub fn route(&self, token: &str, body: &[u8]) -> Result<usize, HookRouteError> {
        let value: serde_json::Value =
            serde_json::from_slice(body).map_err(|e| HookRouteError::BadRequest(e.to_string()))?;
        let mut entries = self.entries.lock();
        let Some(entry) = entries.get_mut(token) else {
            return Err(HookRouteError::UnknownToken);
        };
        let events = (entry.map)(&value);
        let count = events.len();
        for event in events {
            let _ = entry.tx.send(event);
        }
        Ok(count)
    }
}

/// Deregisters a hook token when dropped.
pub struct HookRegistration {
    entries: Arc<Mutex<HashMap<String, HookEntry>>>,
    token: String,
}

impl Drop for HookRegistration {
    fn drop(&mut self) {
        self.entries.lock().remove(&self.token);
    }
}

/// Why a hook delivery was not routed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookRouteError {
    /// The daemon has no hook endpoint configured (e.g. unit tests).
    Disabled,
    /// No session registered this token.
    UnknownToken,
    /// The body was not valid JSON.
    BadRequest(String),
}

impl fmt::Display for HookRouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => write!(f, "agent hooks are disabled"),
            Self::UnknownToken => write!(f, "unknown agent hook token"),
            Self::BadRequest(message) => write!(f, "invalid agent hook payload: {message}"),
        }
    }
}

impl std::error::Error for HookRouteError {}

/// The daemon's configured hook receiver: a router plus the launch endpoint.
#[derive(Debug)]
pub struct HookRuntime {
    pub router: Arc<HookRouter>,
    pub launch: AgentHookLaunch,
}

/// Hook sub-routes. Merged into the daemon's axum app; the handler pulls shared
/// state via `State<Arc<State>>` and forwards to the session's router.
pub fn routes() -> Router<Arc<State>> {
    Router::new().route("/agent-hooks/claude/{token}", post(claude_hook))
}

async fn claude_hook(
    AxumState(state): AxumState<Arc<State>>,
    Path(token): Path<String>,
    body: Bytes,
) -> Response {
    match state.agents.route_hook(&token, &body) {
        // Observe-only: acknowledge without a permission decision.
        Ok(_) => (StatusCode::OK, axum::Json(serde_json::json!({}))).into_response(),
        Err(HookRouteError::BadRequest(message)) => {
            (StatusCode::BAD_REQUEST, message).into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, "unknown agent hook token").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_token_receives_mapped_events() {
        let router = HookRouter::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let map: HookMap = Box::new(|value: &serde_json::Value| {
            vec![AgentStateEvent::Idle {
                outcome: favetto_core::model::IdleOutcome::Succeeded,
            }]
            .into_iter()
            .filter(|_| value.get("ok").is_some())
            .collect()
        });
        let _registration = router.register("tok".to_string(), tx, map);

        assert_eq!(
            router.route("tok", br#"{"ok":true}"#).unwrap(),
            1,
            "the mapped event count is returned"
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            AgentStateEvent::Idle {
                outcome: favetto_core::model::IdleOutcome::Succeeded
            }
        );

        // The guard removes the token: a second registration is needed to route.
        drop(_registration);
        assert_eq!(
            router.route("tok", br#"{"ok":true}"#),
            Err(HookRouteError::UnknownToken)
        );
    }

    #[test]
    fn unknown_token_is_rejected() {
        let router = HookRouter::new();
        assert_eq!(
            router.route("missing", br#"{}"#),
            Err(HookRouteError::UnknownToken)
        );
    }

    #[test]
    fn invalid_json_is_rejected() {
        let router = HookRouter::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        let _registration = router.register("tok".to_string(), tx, Box::new(|_| Vec::new()));
        assert!(matches!(
            router.route("tok", b"not json"),
            Err(HookRouteError::BadRequest(_))
        ));
    }

    #[test]
    fn handler_returns_200_without_a_decision() {
        // The pure router path ack's with the event count and never sends a
        // decision anywhere: the observe-only contract.
        let router = HookRouter::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _registration = router.register(
            "tok".to_string(),
            tx,
            Box::new(|_| vec![AgentStateEvent::TurnStarted]),
        );
        assert_eq!(
            router
                .route("tok", br#"{"hook_event_name":"UserPromptSubmit"}"#)
                .unwrap(),
            1
        );
        assert!(matches!(rx.try_recv(), Ok(AgentStateEvent::TurnStarted)));
        assert!(rx.try_recv().is_err());
    }
}
