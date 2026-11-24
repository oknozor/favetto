//! Session-scoped ACP authentication actions.
//!
//! The daemon only accepts `agents.authenticate` while a session reports
//! `required` or `failed`, and only accepts `agents.logout` while it reports
//! `not_required`. The advertised method list outlives the auth state (it is
//! kept so a settled session can still sign out), so the UI must base its
//! actions on the state machine rather than on the presence of methods.
//!
//! Both the conversation banner and the Settings card use these helpers so the
//! rule cannot drift between surfaces.

use favetto_wire::model::{AgentAuthMethod, AgentAuthMethodKind, AgentAuthState, AgentAuthStatus};

/// Whether the daemon would accept a sign-in right now.
///
/// Mirrors the daemon guard: only a paused (`required`) or retryable (`failed`)
/// session may authenticate. `not_required`, `authenticating` and the transient
/// `authenticated` state must not offer the action.
pub fn can_sign_in(auth: &AgentAuthState) -> bool {
    matches!(
        auth.status,
        AgentAuthStatus::Required | AgentAuthStatus::Failed
    )
}

/// Whether the daemon would accept a sign-out right now.
///
/// The daemon only permits `agents.logout` once the session has settled in
/// `not_required`, and only when the agent advertised the capability.
pub fn can_sign_out(auth: &AgentAuthState) -> bool {
    auth.status == AgentAuthStatus::NotRequired && auth.logout_supported
}

/// The agent-handled methods the current state may offer as sign-in actions.
///
/// This is empty for every state that cannot authenticate, and for terminal
/// methods that the daemon cannot run on a remote client's behalf.
pub fn sign_in_methods(auth: &AgentAuthState) -> Vec<&AgentAuthMethod> {
    if !can_sign_in(auth) {
        return Vec::new();
    }
    auth.methods
        .iter()
        .filter(|method| method.supported)
        .collect()
}

/// Whether the conversation banner has anything to show.
///
/// In-progress states always surface progress; a settled session only keeps a
/// banner while a sign-out is available.
pub fn banner_visible(auth: &AgentAuthState) -> bool {
    matches!(
        auth.status,
        AgentAuthStatus::Required | AgentAuthStatus::Authenticating | AgentAuthStatus::Failed
    ) || can_sign_out(auth)
}

/// Whether the agent advertised only terminal methods.
///
/// Those stay visible as guidance but are never actionable for a remote client.
pub fn terminal_only(auth: &AgentAuthState) -> bool {
    !auth.methods.iter().any(|method| method.supported)
        && auth
            .methods
            .iter()
            .any(|method| method.kind == AgentAuthMethodKind::Terminal)
}

/// One read-only auth status row in the Settings Agents card.
///
/// Settings never exposes per-session authentication controls: it carries only
/// text and points the user at the session surface where the action belongs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthStatusRow {
    pub headline: &'static str,
    pub detail: String,
}

/// Build the read-only Settings row for a session's auth state.
///
/// Settings has no per-session controls and the session id is an internal
/// routing key, so neither the headline nor the detail may contain it.
pub fn settings_row(auth: &AgentAuthState) -> AuthStatusRow {
    let headline = match auth.status {
        AgentAuthStatus::Required => "needs sign-in",
        AgentAuthStatus::Authenticating => "signing in…",
        AgentAuthStatus::Failed => "sign-in failed",
        AgentAuthStatus::Authenticated => "signed in",
        AgentAuthStatus::NotRequired => "signed in",
    };
    let detail = auth
        .message
        .clone()
        .or_else(|| {
            terminal_only(auth).then(|| {
                "Terminal sign-in cannot run remotely; configure credentials on the daemon host."
                    .to_string()
            })
        })
        .unwrap_or_else(|| match auth.status {
            AgentAuthStatus::Required | AgentAuthStatus::Failed => {
                "sign in from the task details or Agent panel".to_string()
            }
            AgentAuthStatus::Authenticating | AgentAuthStatus::Authenticated => {
                "the daemon is completing the agent's own sign-in flow".to_string()
            }
            AgentAuthStatus::NotRequired if auth.logout_supported => {
                "sign out from the task details or Agent panel".to_string()
            }
            AgentAuthStatus::NotRequired => "no action needed".to_string(),
        });
    AuthStatusRow { headline, detail }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn method(id: &str, kind: AgentAuthMethodKind, supported: bool) -> AgentAuthMethod {
        AgentAuthMethod {
            id: id.to_string(),
            name: id.to_string(),
            description: None,
            kind,
            supported,
        }
    }

    fn auth(status: AgentAuthStatus, logout_supported: bool) -> AgentAuthState {
        AgentAuthState {
            status,
            methods: vec![method("agent-login", AgentAuthMethodKind::Agent, true)],
            logout_supported,
            method_id: None,
            message: None,
        }
    }

    const ALL_STATUSES: [AgentAuthStatus; 5] = [
        AgentAuthStatus::NotRequired,
        AgentAuthStatus::Required,
        AgentAuthStatus::Authenticating,
        AgentAuthStatus::Authenticated,
        AgentAuthStatus::Failed,
    ];

    #[test]
    fn not_required_without_logout_offers_nothing_and_hides_the_banner() {
        let auth = auth(AgentAuthStatus::NotRequired, false);
        assert!(!can_sign_in(&auth));
        assert!(sign_in_methods(&auth).is_empty());
        assert!(!can_sign_out(&auth));
        assert!(!banner_visible(&auth));
    }

    #[test]
    fn not_required_with_logout_offers_only_sign_out() {
        let auth = auth(AgentAuthStatus::NotRequired, true);
        assert!(sign_in_methods(&auth).is_empty());
        assert!(can_sign_out(&auth));
        assert!(banner_visible(&auth));
    }

    #[test]
    fn required_offers_supported_methods_but_no_sign_out() {
        let auth = auth(AgentAuthStatus::Required, true);
        let methods = sign_in_methods(&auth);
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].id, "agent-login");
        assert!(!can_sign_out(&auth));
        assert!(banner_visible(&auth));
    }

    #[test]
    fn terminal_methods_are_never_actionable() {
        let mut auth = auth(AgentAuthStatus::Required, false);
        auth.methods = vec![method(
            "terminal-login",
            AgentAuthMethodKind::Terminal,
            false,
        )];
        assert!(sign_in_methods(&auth).is_empty());
        assert!(terminal_only(&auth));
    }

    #[test]
    fn authenticating_shows_progress_without_actions() {
        let auth = auth(AgentAuthStatus::Authenticating, true);
        assert!(sign_in_methods(&auth).is_empty());
        assert!(!can_sign_out(&auth));
        assert!(banner_visible(&auth));
    }

    #[test]
    fn authenticated_offers_nothing_while_setup_is_retried() {
        let auth = auth(AgentAuthStatus::Authenticated, true);
        assert!(sign_in_methods(&auth).is_empty());
        assert!(!can_sign_out(&auth));
        assert!(!banner_visible(&auth));
    }

    #[test]
    fn failed_offers_supported_methods_for_retry() {
        let auth = auth(AgentAuthStatus::Failed, true);
        assert_eq!(sign_in_methods(&auth).len(), 1);
        assert!(!can_sign_out(&auth));
        assert!(banner_visible(&auth));
    }

    #[test]
    fn settings_rows_are_read_only_for_every_status() {
        for status in ALL_STATUSES {
            for logout_supported in [false, true] {
                let auth = auth(status, logout_supported);
                let row = settings_row(&auth);
                assert!(!row.headline.contains("Sign in"), "{:?}", row);
                assert!(!row.headline.contains("Sign out"), "{:?}", row);
                assert!(!row.detail.contains("Sign in with"), "{:?}", row);
                assert!(!row.detail.contains("Sign out"), "{:?}", row);
            }
        }
    }

    #[test]
    fn settings_points_at_the_contextual_surface() {
        let auth = auth(AgentAuthStatus::Required, false);
        let row = settings_row(&auth);
        assert_eq!(row.headline, "needs sign-in");
        assert!(row.detail.contains("task details or Agent panel"));
    }

    #[test]
    fn settings_rows_never_expose_the_session_id() {
        for status in ALL_STATUSES {
            for logout_supported in [false, true] {
                let auth = auth(status, logout_supported);
                let row = settings_row(&auth);
                assert!(!row.detail.contains("ses_"), "{:?}", row);
                assert!(!row.detail.contains("session "), "{:?}", row);
            }
        }
    }
}
