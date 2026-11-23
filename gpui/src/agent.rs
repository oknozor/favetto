//! The free-standing Agent surface's state.
//!
//! Unlike [`crate::detail::TaskDetail`], a standalone agent session is not tied
//! to a catalog task or run. It wraps the shared [`Conversation`] model and adds
//! only the start form, the chosen agent/model, the session lifecycle and the
//! active repository checkout. The daemon call is `agents.start` with no
//! `task_id`; the cwd is always the active repository's main working tree.

use std::path::PathBuf;

use favetto_wire::model::AgentSessionInfo;
use favetto_wire::rpc::{AgentAttachResult, AgentEventPush, AgentExitPush};

use crate::conversation::Conversation;
use crate::text_input::TextInput;

/// The standalone Agent panel state. One lives on
/// [`crate::state::ClientState`] for the panel's lifetime, so switching routes
/// never loses the live session or its in-memory feed.
#[derive(Clone, Debug)]
pub struct StandaloneAgent {
    /// The shared conversation model (feed, plan, commands, composer, prompts).
    pub conversation: Conversation,
    /// The active Favetto-local session id, once `agents.start` returned.
    pub session_id: Option<String>,
    /// The last attach result's session, kept until `agents.list` catches up.
    pub session: Option<AgentSessionInfo>,
    /// The configured agent chosen in the picker.
    pub chosen_agent: Option<String>,
    /// The model override typed before the session starts.
    pub model: TextInput,
    /// Whether the model field owns the keyboard (a click focuses it).
    pub model_focused: bool,
    /// The active session's main-checkout cwd, frozen at start.
    pub cwd: Option<PathBuf>,
    /// The active repo name at start, frozen with the cwd.
    pub repo: Option<String>,
    /// The cwd/repo a start in flight will use.
    pub pending_cwd: Option<PathBuf>,
    /// The repo a start in flight will use.
    pub pending_repo: Option<String>,
    /// Whether an `agents.start` is in flight.
    pub starting: bool,
    /// The last start failure, shown in the panel.
    pub start_error: Option<String>,
    /// Whether an `agents.close` is in flight.
    pub closing: bool,
    /// The last close failure, shown in the panel.
    pub close_error: Option<String>,
    /// The exit push for the active session, which leaves the composer disabled.
    pub exit: Option<AgentExitPush>,
}

impl Default for StandaloneAgent {
    fn default() -> Self {
        Self {
            conversation: Conversation::for_session(String::new()),
            session_id: None,
            session: None,
            chosen_agent: None,
            model: TextInput::default(),
            model_focused: false,
            cwd: None,
            repo: None,
            pending_cwd: None,
            pending_repo: None,
            starting: false,
            start_error: None,
            closing: false,
            close_error: None,
            exit: None,
        }
    }
}

impl StandaloneAgent {
    /// Whether an event belongs to the standalone conversation.
    pub fn accepts_event(&self, event: &AgentEventPush) -> bool {
        let Some(session_id) = self.session_id.as_deref() else {
            return false;
        };
        event.session_id == session_id
            || self.conversation.events_session.as_deref() == Some(session_id)
    }

    /// Adopt one `agents.start` / `agents.attach` result and seed the shared
    /// conversation from its cursor, plan and commands. Returns the previous
    /// session id so a replacing start can close it.
    pub fn adopt(&mut self, attached: AgentAttachResult) -> Option<String> {
        let previous = self.session_id.clone();
        let session = attached.session;
        let mut conversation = Conversation::for_session(session.id.clone());
        conversation.events_cursor = attached.seq;
        if !attached.plan.is_empty() {
            conversation.plan = Some(attached.plan);
        }
        if !attached.commands.is_empty() {
            conversation.commands = Some(attached.commands);
        }
        conversation.mark_status(!session.running, session.awaiting_input.is_some());
        self.session_id = Some(session.id.clone());
        self.session = Some(session);
        self.conversation = conversation;
        self.cwd = self.pending_cwd.clone();
        self.repo = self.pending_repo.clone();
        self.starting = false;
        self.start_error = None;
        self.closing = false;
        self.close_error = None;
        self.model_focused = false;
        self.exit = None;
        previous.filter(|id| self.session_id.as_deref() != Some(id.as_str()))
    }

    /// Fold one roster snapshot into the lifecycle flags. The roster is
    /// authoritative after the attach result.
    pub fn sync_status(&mut self, session: Option<&AgentSessionInfo>) {
        if let Some(session) = session {
            self.session = Some(session.clone());
        }
        let terminal = self.exit.is_some() || session.is_some_and(|session| !session.running);
        let awaiting = session
            .and_then(|session| session.awaiting_input.as_ref())
            .is_some();
        self.conversation.mark_status(terminal, awaiting);
    }

    /// Record an `agent.exit` push for the active session.
    pub fn record_exit(&mut self, exit: AgentExitPush) -> bool {
        if self.session_id.as_deref() != Some(exit.session_id.as_str()) {
            return false;
        }
        self.exit = Some(exit);
        self.conversation.mark_status(true, false);
        self.conversation.turn_cancel_pending = None;
        true
    }

    /// Forget the active session (after a close or when replacing it).
    pub fn clear(&mut self) {
        self.conversation = Conversation::for_session(String::new());
        self.session_id = None;
        self.session = None;
        self.cwd = None;
        self.repo = None;
        self.pending_cwd = None;
        self.pending_repo = None;
        self.starting = false;
        self.start_error = None;
        self.closing = false;
        self.close_error = None;
        self.exit = None;
        self.model_focused = false;
    }
}

/// Split an optional model override into the `provider`/`model` pair accepted by
/// `agents.start`. A bare id stays a model; `provider/model` is split once. This
/// matches the Catalog launch sheet's accepted syntax.
pub fn model_override(value: &str) -> (Option<String>, Option<String>) {
    let value = value.trim();
    if value.is_empty() {
        return (None, None);
    }
    match value.split_once('/') {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
            (Some(provider.to_string()), Some(model.to_string()))
        }
        _ => (None, Some(value.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use favetto_wire::model::{AwaitingInputKind, AwaitingInputReason};

    fn info(id: &str, running: bool) -> AgentSessionInfo {
        AgentSessionInfo {
            id: id.to_string(),
            agent: "opencode".to_string(),
            backend: Default::default(),
            task_id: None,
            running,
            headless: false,
            session_id: None,
            awaiting_input: None,
            activity: None,
            usage: None,
            load_state: Default::default(),
            load_error: None,
            config_options: Vec::new(),
            modes: None,
            auth: None,
            mcp: None,
        }
    }

    fn attached(id: &str) -> AgentAttachResult {
        AgentAttachResult {
            session: info(id, true),
            data: String::new(),
            seq: 12,
            plan: vec![favetto_wire::model::AgentPlanEntry {
                content: "inspect".to_string(),
                ..Default::default()
            }],
            commands: vec![favetto_wire::model::AgentCommand {
                name: "web".to_string(),
                description: "Search".to_string(),
                input_hint: None,
            }],
        }
    }

    #[test]
    fn model_override_accepts_a_bare_id_or_provider_model() {
        assert_eq!(model_override(""), (None, None));
        assert_eq!(
            model_override("deepseek"),
            (None, Some("deepseek".to_string()))
        );
        assert_eq!(
            model_override("opencode-go/deepseek"),
            (
                Some("opencode-go".to_string()),
                Some("deepseek".to_string())
            )
        );
        assert_eq!(model_override(" bare "), (None, Some("bare".to_string())));
    }

    #[test]
    fn adopting_a_start_result_seeds_the_conversation_and_returns_the_old_session() {
        let mut agent = StandaloneAgent {
            session_id: Some("old".to_string()),
            pending_cwd: Some(PathBuf::from("/repo")),
            pending_repo: Some("favetto".to_string()),
            ..StandaloneAgent::default()
        };
        let previous = agent.adopt(attached("new"));
        assert_eq!(previous.as_deref(), Some("old"));
        assert_eq!(agent.session_id.as_deref(), Some("new"));
        assert_eq!(agent.conversation.events_cursor, 12);
        assert_eq!(agent.conversation.plan().len(), 1);
        assert_eq!(agent.conversation.commands().len(), 1);
        assert_eq!(agent.cwd, Some(PathBuf::from("/repo")));
        assert!(!agent.starting);
    }

    #[test]
    fn an_exit_leaves_the_session_terminal_and_disables_input() {
        let mut agent = StandaloneAgent {
            session_id: Some("ses".to_string()),
            ..StandaloneAgent::default()
        };
        agent.conversation.mark_status(false, false);
        assert!(agent.record_exit(AgentExitPush {
            session_id: "ses".to_string(),
            code: Some(1),
        }));
        assert!(agent.conversation.terminal);
        assert!(!agent.record_exit(AgentExitPush {
            session_id: "other".to_string(),
            code: None,
        }));
    }

    #[test]
    fn a_roster_awaiting_input_marks_the_conversation_blocked() {
        let mut agent = StandaloneAgent::default();
        let mut session = info("ses", true);
        session.awaiting_input = Some(AwaitingInputReason {
            kind: AwaitingInputKind::Permission,
            message: "Allow?".to_string(),
            request_id: Some("perm".to_string()),
            options: Vec::new(),
            allow_always: false,
            form: None,
        });
        agent.sync_status(Some(&session));
        assert!(!agent.conversation.terminal);
        assert!(agent.conversation.awaiting_input);

        let mut stopped = info("ses", false);
        stopped.load_state = Default::default();
        agent.sync_status(Some(&stopped));
        assert!(agent.conversation.terminal);
        assert!(!agent.session.as_ref().expect("session").running);
    }
}
