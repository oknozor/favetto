//! The free-standing Agent surface's state.
//!
//! Unlike [`crate::detail::TaskDetail`], a standalone agent session is not tied
//! to a catalog task or run. It wraps the shared [`Conversation`] model and adds
//! only the start form, the chosen agent/model, the session lifecycle and the
//! active repository checkout. The daemon call is `agents.start` with no
//! `task_id`; the cwd is always the active repository's main working tree.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use favetto_wire::model::AgentSessionInfo;
use favetto_wire::rpc::{AgentAttachResult, AgentEventPush, AgentExitPush};

use crate::conversation::Conversation;
use crate::detail::{restored_session, SessionRestore};
use crate::session_menu::SessionMenu;
use crate::text_input::TextInput;

/// How many inactive session conversations the panel keeps alive. Switching
/// back restores the feed, activity and composer draft without re-fetching.
const MAX_CACHED_CONVERSATIONS: usize = 8;

/// The standalone Agent panel state. One lives on
/// [`crate::state::ClientState`] for the panel's lifetime, so switching routes
/// never loses the live session or its in-memory feed.
#[derive(Clone, Debug)]
pub struct StandaloneAgent {
    /// The shared conversation model (feed, plan, commands, composer, prompts).
    pub conversation: Conversation,
    /// Inactive per-session conversations, bounded and keyed by local id.
    conversations: HashMap<String, Conversation>,
    /// Read-only transcripts for sessions this client never watched, kept with
    /// the inactive conversation so switching back never shows a blank panel.
    restores: HashMap<String, SessionRestore>,
    /// Least-recently used cache order.
    cache_order: VecDeque<String>,
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
    /// The on-demand live + history session switcher.
    pub session_menu: SessionMenu,
    /// The explicit switch target whose attach/load reply is in flight.
    pub pending_switch: Option<String>,
    /// The last switch failure, shown without replacing the current feed.
    pub switch_error: Option<String>,
    /// A read-only transcript for a session this client never watched.
    pub restore: Option<SessionRestore>,
}

impl Default for StandaloneAgent {
    fn default() -> Self {
        Self {
            conversation: Conversation::for_session(String::new()),
            conversations: HashMap::new(),
            restores: HashMap::new(),
            cache_order: VecDeque::new(),
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
            session_menu: SessionMenu::default(),
            pending_switch: None,
            switch_error: None,
            restore: None,
        }
    }
}

impl StandaloneAgent {
    /// Whether an event belongs to the selected standalone conversation.
    pub fn accepts_event(&self, event: &AgentEventPush, sessions: &[AgentSessionInfo]) -> bool {
        self.session_id.as_deref() == Some(event.session_id.as_str())
            && self.conversation.accepts_event(event, sessions)
    }

    /// Fold a live event into the selected conversation, or into a cached
    /// inactive one. Returns whether the selected conversation changed.
    pub fn fold_event(&mut self, event: &AgentEventPush, sessions: &[AgentSessionInfo]) -> bool {
        if self.accepts_event(event, sessions) {
            self.conversation.push_event(event.clone());
            return true;
        }
        if let Some(conversation) = self
            .conversations
            .values_mut()
            .find(|conversation| conversation.accepts_event(event, sessions))
        {
            conversation.push_event(event.clone());
        }
        false
    }

    /// Fold an `agent.state` activity into the matching active or cached
    /// conversation. Inactive sessions never touch the selected feed.
    pub fn fold_activity(
        &mut self,
        session_id: &str,
        activity: Option<favetto_wire::model::AgentActivity>,
        sessions: &[AgentSessionInfo],
    ) {
        if self.conversation.owns_session(session_id, sessions)
            && self.session_id.as_deref() == Some(session_id)
        {
            self.conversation.apply_activity(activity.clone());
        }
        if let Some(conversation) = self.conversations.get_mut(session_id) {
            if conversation.owns_session(session_id, sessions) {
                conversation.apply_activity(activity);
            }
        }
    }

    /// Move the active conversation into the bounded cache.
    fn park_active(&mut self) {
        let Some(session_id) = self.session_id.clone() else {
            return;
        };
        if session_id.is_empty() {
            return;
        }
        let conversation = std::mem::replace(
            &mut self.conversation,
            Conversation::for_session(String::new()),
        );
        if let Some(restore) = self.restore.take() {
            self.restores.insert(session_id.clone(), restore);
        }
        self.cache_insert(session_id, conversation);
    }

    fn cache_insert(&mut self, session_id: String, conversation: Conversation) {
        if self
            .conversations
            .insert(session_id.clone(), conversation)
            .is_some()
        {
            self.cache_order.retain(|id| id != &session_id);
        }
        self.cache_order.push_back(session_id);
        while self.cache_order.len() > MAX_CACHED_CONVERSATIONS {
            let Some(oldest) = self.cache_order.pop_front() else {
                break;
            };
            if self.session_id.as_deref() != Some(oldest.as_str()) {
                self.conversations.remove(&oldest);
                self.restores.remove(&oldest);
            }
        }
    }

    /// Remove a session from the inactive cache (after an explicit close).
    pub fn forget(&mut self, session_id: &str) {
        self.conversations.remove(session_id);
        self.restores.remove(session_id);
        self.cache_order.retain(|id| id != session_id);
    }

    /// Adopt one `agents.start` result and seed a fresh conversation from its
    /// cursor, plan and commands. Returns the previous session id so a replacing
    /// start can close it. Every stale feed/routing field is reset.
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
        if previous.as_deref() != Some(session.id.as_str()) {
            self.park_active();
        }
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
        self.session_menu.close();
        self.pending_switch = None;
        self.switch_error = None;
        self.restore = None;
        previous.filter(|id| self.session_id.as_deref() != Some(id.as_str()))
    }

    /// Adopt one explicit `agents.attach` / `agents.load` result.
    ///
    /// A conversation this panel already watched is restored from the cache so
    /// its feed, activity and draft survive. A never-watched session gets a
    /// fresh conversation seeded from the snapshot plus a read-only transcript.
    pub fn switch_to(&mut self, attached: AgentAttachResult) -> bool {
        let session = attached.session.clone();
        let session_id = session.id.clone();
        let cached = self.conversations.remove(&session_id);
        let cached_restore = self.restores.remove(&session_id);
        self.cache_order.retain(|id| id != &session_id);
        let previous = self.session_id.clone();
        let same = previous.as_deref() == Some(session_id.as_str());
        if !same {
            self.park_active();
        }

        let mut restore = cached_restore;
        match cached {
            Some(conversation) => self.conversation = conversation,
            None if same => restore = self.restore.take(),
            None => {
                let mut conversation = Conversation::for_session(session_id.clone());
                conversation.events_cursor = attached.seq;
                if !attached.plan.is_empty() {
                    conversation.plan = Some(attached.plan.clone());
                }
                if !attached.commands.is_empty() {
                    conversation.commands = Some(attached.commands.clone());
                }
                match restored_session(attached, &session_id) {
                    Ok(restored) => {
                        // The attach snapshot carries no structured event
                        // backfill, so the labelled transcript is the readable
                        // history for a session this client never watched.
                        restore = Some(SessionRestore::Restored(restored));
                    }
                    Err(message) => restore = Some(SessionRestore::Failed(message)),
                }
                self.conversation = conversation;
            }
        }
        self.conversation
            .mark_status(!session.running, session.awaiting_input.is_some());
        self.session_id = Some(session_id);
        self.session = Some(session);
        self.starting = false;
        self.start_error = None;
        self.closing = false;
        self.close_error = None;
        self.model_focused = false;
        self.exit = None;
        self.session_menu.close();
        self.pending_switch = None;
        self.switch_error = None;
        self.restore = restore;
        !same
    }

    /// Set the active location from the chosen history row (or clear it when
    /// the daemon did not report one).
    pub fn set_location(&mut self, cwd: Option<PathBuf>, repo: Option<String>) {
        self.cwd = cwd;
        self.repo = repo;
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
            if let Some(conversation) = self.conversations.get_mut(&exit.session_id) {
                conversation.mark_status(true, false);
            }
            return false;
        }
        self.exit = Some(exit);
        self.conversation.mark_status(true, false);
        self.conversation.turn_cancel_pending = None;
        true
    }

    /// Forget the active session (after a close or when replacing it).
    pub fn clear(&mut self) {
        if let Some(session_id) = self.session_id.clone() {
            self.forget(&session_id);
        }
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
        self.pending_switch = None;
        self.switch_error = None;
        self.restore = None;
        self.session_menu.close();
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
    use favetto_wire::model::{
        AgentActivity, AgentStateEvent, AwaitingInputKind, AwaitingInputReason,
    };

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

    fn event(seq: u64, session_id: &str, text: &str) -> AgentEventPush {
        AgentEventPush {
            session_id: session_id.to_string(),
            task_id: None,
            seq,
            event: AgentStateEvent::TextDelta {
                role: favetto_wire::model::MessageRole::Assistant,
                text: text.to_string(),
            },
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
    fn a_standalone_conversation_ignores_a_task_backed_sessions_events() {
        let mut agent = StandaloneAgent::default();
        agent.adopt(attached("ses_a"));
        let sessions = vec![info("ses_a", true), info("task-session", true)];

        // The first event is the standalone session's own.
        assert!(agent.fold_event(&event(13, "ses_a", "standalone"), &sessions));
        assert_eq!(agent.conversation.events.len(), 1);

        // A task the agent launched runs its own headless session. Its events
        // must not enter the selected feed.
        assert!(!agent.fold_event(&event(1, "task-session", "spawned task output"), &sessions));
        assert_eq!(agent.conversation.events.len(), 1);
        assert_eq!(agent.conversation.events_session.as_deref(), Some("ses_a"));
    }

    #[test]
    fn switching_back_restores_the_feed_activity_and_composer_draft() {
        let mut agent = StandaloneAgent::default();
        agent.adopt(attached("ses_a"));
        let sessions = vec![info("ses_a", true), info("ses_b", true)];
        assert!(agent.fold_event(&event(13, "ses_a", "hello"), &sessions));
        agent
            .conversation
            .prompt
            .set_value("draft for a".to_string());
        agent.conversation.activity = Some(AgentActivity::Thinking);

        assert!(agent.switch_to(attached("ses_b")));
        assert_eq!(agent.session_id.as_deref(), Some("ses_b"));
        assert!(agent.conversation.events.is_empty());
        assert!(agent.conversation.prompt.is_empty());

        assert!(agent.switch_to(attached("ses_a")));
        assert_eq!(agent.session_id.as_deref(), Some("ses_a"));
        assert_eq!(agent.conversation.events.len(), 1);
        assert_eq!(agent.conversation.prompt.value(), "draft for a");
        assert_eq!(agent.conversation.activity, Some(AgentActivity::Thinking));
    }

    #[test]
    fn a_never_watched_transcript_survives_switching_away_and_back() {
        use base64::Engine as _;
        let mut first = attached("past");
        first.data = base64::engine::general_purpose::STANDARD.encode("old conversation");
        first.seq = 4;
        let mut agent = StandaloneAgent::default();
        assert!(agent.switch_to(first));
        assert!(matches!(
            agent.restore,
            Some(SessionRestore::Restored(ref restored))
                if restored.transcript == "old conversation"
        ));

        assert!(agent.switch_to(attached("live")));
        assert!(agent.switch_to(attached("past")));
        assert!(matches!(
            agent.restore,
            Some(SessionRestore::Restored(ref restored))
                if restored.transcript == "old conversation"
        ));
        assert!(agent.conversation.events.is_empty());
    }

    #[test]
    fn adopting_a_new_start_resets_stale_routing_and_the_menu() {
        let mut agent = StandaloneAgent::default();
        agent.adopt(attached("old"));
        agent.conversation.push_event(event(1, "old", "stale"));
        agent.pending_switch = Some("old".to_string());
        agent.session_menu.open();

        let previous = agent.adopt(attached("new"));
        assert_eq!(previous.as_deref(), Some("old"));
        assert_eq!(agent.session_id.as_deref(), Some("new"));
        assert_eq!(agent.conversation.events_session, None);
        assert!(agent.conversation.events.is_empty());
        assert!(agent.pending_switch.is_none());
        assert!(!agent.session_menu.is_open());
        assert!(agent.switch_error.is_none());
        assert!(agent.restore.is_none());
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
