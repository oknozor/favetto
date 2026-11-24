//! The task-neutral conversation model shared by the task detail and the Agent
//! panel.
//!
//! A conversation is addressed by a [`ConversationTarget`]: a catalog task run
//! or a free-standing Favetto-local agent session. Everything that only depends
//! on the structured event feed — grouping, cursor bookkeeping, plan/command
//! snapshots, the prompt draft and the permission/elicitation correlation — lives
//! here. [`crate::detail::TaskDetail`] and [`crate::agent::StandaloneAgent`] both
//! wrap this model and add only their own surface-specific state.

use std::collections::HashMap;

use favetto_wire::model::{
    AgentCommand, AgentPlanEntry, AgentSessionInfo, AgentStateEvent, AwaitingInputKind,
};
use favetto_wire::rpc::AgentEventPush;
use uuid::Uuid;

use crate::detail::{
    build_feed, command_query, matching_commands, FeedItem, PendingElicitation, PendingRequest,
    MAX_RETAINED_EVENTS,
};
use crate::text_input::TextInput;

/// What a shared conversation belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ConversationTarget {
    /// A catalog task run.
    Task(Uuid),
    /// A free-standing agent session, keyed by its Favetto-local id.
    Session(String),
}

impl ConversationTarget {
    /// Whether this target addresses the same session as `session_id`.
    pub fn is_session(&self, session_id: &str) -> bool {
        matches!(self, ConversationTarget::Session(id) if !id.is_empty() && id == session_id)
    }

    /// Whether `session` is the live session behind this target.
    pub fn matches_session(&self, session: &AgentSessionInfo) -> bool {
        match self {
            ConversationTarget::Task(task_id) => {
                session
                    .task_id
                    .as_deref()
                    .and_then(|id| Uuid::parse_str(id).ok())
                    == Some(*task_id)
            }
            ConversationTarget::Session(session_id) => {
                !session_id.is_empty() && session.id == *session_id
            }
        }
    }
}

/// The task-neutral structured conversation.
#[derive(Clone, Debug)]
pub struct Conversation {
    /// The run or free-standing session this conversation belongs to.
    pub target: ConversationTarget,
    /// The structured feed, ordered by `seq`.
    pub events: Vec<AgentEventPush>,
    /// The session the daemon resolved for the feed.
    pub events_session: Option<String>,
    /// The highest seen per-session `seq`.
    pub events_cursor: u64,
    /// Event sequences already folded into a restored transcript. Feed rows at
    /// or below this floor are dropped instead of painting the same text twice.
    pub events_floor: u64,
    /// Whether the first backfill is in flight.
    pub events_loading: bool,
    /// The last backfill failure.
    pub events_error: Option<String>,
    /// Whether at least one backfill completed (an empty feed is then real).
    pub events_loaded: bool,
    /// Event kinds skipped because this client does not know them.
    pub events_ignored: usize,
    /// Monotonic revision of [`Conversation::events`], used by the virtualized
    /// feed to rebuild its item list only when the events actually changed.
    pub events_revision: u64,
    /// Whether new content should keep the viewport at the tail.
    pub follow: bool,
    /// Events that arrived while the viewport was pinned off the tail.
    pub new_events: u64,
    /// The request id with an in-flight `agents.reply`.
    pub reply_pending: Option<String>,
    /// The edited form for the current elicitation request.
    pub elicitation: Option<crate::elicitation::ElicitationFormState>,
    /// The elicitation request id with an in-flight reply.
    pub elicitation_pending: Option<String>,
    /// The elicitation id most recently answered, hidden until a new one arrives.
    pub answered_elicitation: Option<String>,
    /// The live session id whose turn has an in-flight/cancelling
    /// `agents.cancel`. Cleared when the turn reports idle or the RPC fails.
    pub turn_cancel_pending: Option<String>,
    /// The request id most recently answered, hidden until a new request arrives.
    pub answered_request: Option<String>,
    /// The current execution plan snapshot.
    pub plan: Option<Vec<AgentPlanEntry>>,
    /// The current advertised command snapshot.
    pub commands: Option<Vec<AgentCommand>>,
    /// The follow-up prompt draft.
    pub prompt: TextInput,
    /// The highlighted command completion in the prompt.
    pub prompt_selected: Option<usize>,
    /// Whether an `agents.prompt` is in flight.
    pub prompt_sending: bool,
    /// Whether the follow-up composer is open and owns the keyboard.
    pub prompt_focused: bool,
    /// The last prompt-send failure.
    pub prompt_error: Option<String>,
    /// Whether the conversation can no longer change.
    pub terminal: bool,
    /// Whether the owning row says the agent is blocked on input.
    pub awaiting_input: bool,
}

impl Conversation {
    /// A conversation addressed by `target`.
    pub fn new(target: ConversationTarget) -> Self {
        Self {
            target,
            events: Vec::new(),
            events_session: None,
            events_cursor: 0,
            events_floor: 0,
            events_loading: false,
            events_error: None,
            events_loaded: false,
            events_ignored: 0,
            events_revision: 0,
            follow: true,
            new_events: 0,
            reply_pending: None,
            elicitation: None,
            elicitation_pending: None,
            answered_elicitation: None,
            turn_cancel_pending: None,
            answered_request: None,
            plan: None,
            commands: None,
            prompt: TextInput::default(),
            prompt_selected: None,
            prompt_sending: false,
            prompt_focused: false,
            prompt_error: None,
            terminal: false,
            awaiting_input: false,
        }
    }

    /// A task-run conversation.
    pub fn for_task(task_id: Uuid) -> Self {
        Self::new(ConversationTarget::Task(task_id))
    }

    /// A free-standing session conversation.
    pub fn for_session(session_id: impl Into<String>) -> Self {
        Self::new(ConversationTarget::Session(session_id.into()))
    }

    /// Update the lifecycle flags derived by the owning surface.
    pub fn mark_status(&mut self, terminal: bool, awaiting_input: bool) {
        self.terminal = terminal;
        self.awaiting_input = awaiting_input;
    }

    /// Whether the always-visible follow-up composer can accept a prompt.
    ///
    /// A finished run/session or a conversation with no attached agent session
    /// keeps the composer on screen but read-only.
    pub fn prompt_enabled(&self, has_session: bool) -> bool {
        has_session && !self.terminal
    }

    /// Clear the feed and every per-conversation draft after a new attempt or a
    /// replacement session.
    pub fn reset_for_new_attempt(&mut self) {
        self.events.clear();
        self.events_session = None;
        self.events_cursor = 0;
        self.events_floor = 0;
        self.events_loaded = false;
        self.events_ignored = 0;
        self.events_revision = self.events_revision.wrapping_add(1);
        self.new_events = 0;
        self.answered_request = None;
        self.reply_pending = None;
        self.turn_cancel_pending = None;
        self.plan = None;
        self.commands = None;
        self.prompt.set_value(String::new());
        self.prompt_selected = None;
        self.prompt_sending = false;
        self.prompt_focused = false;
        self.prompt_error = None;
    }

    /// Drop the previous attempt's feed so a replayed conversation is not
    /// filtered out by the old session's cursor.
    pub fn reset_events_for_restore(&mut self) {
        self.events.clear();
        self.events_session = None;
        self.events_cursor = 0;
        self.events_floor = 0;
        self.events_loaded = false;
        self.events_ignored = 0;
        self.events_revision = self.events_revision.wrapping_add(1);
        self.new_events = 0;
        self.plan = None;
        self.commands = None;
    }

    /// Treat `seq` as the last event already folded into a restored transcript.
    ///
    /// Rows at or below the floor are discarded now and skipped by later
    /// backfills/live pushes, so the transcript and the structured feed cannot
    /// paint the same text twice.
    pub fn restore_events_after(&mut self, seq: u64) {
        self.events_floor = seq;
        let before = self.events.len();
        self.events.retain(|event| event.seq > seq);
        self.events_cursor = self.events_cursor.max(seq);
        if self.events.len() != before {
            self.events_revision = self.events_revision.wrapping_add(1);
        }
    }

    /// Merge a backfill with any live events already received.
    pub fn merge_events(
        &mut self,
        session_id: Option<String>,
        events: Vec<AgentEventPush>,
        next_cursor: u64,
        plan: Option<Vec<AgentPlanEntry>>,
        commands: Option<Vec<AgentCommand>>,
        ignored: usize,
    ) {
        let mut merged: HashMap<(String, u64), AgentEventPush> = HashMap::new();
        for event in self.events.drain(..) {
            merged.insert((event.session_id.clone(), event.seq), event);
        }
        let mut highest = next_cursor;
        let mut latest_session = session_id;
        for event in events {
            if event.seq <= self.events_floor {
                continue;
            }
            highest = highest.max(event.seq);
            if latest_session.is_none() {
                latest_session = Some(event.session_id.clone());
            }
            merged.insert((event.session_id.clone(), event.seq), event);
        }
        let mut events: Vec<AgentEventPush> = merged.into_values().collect();
        events.sort_by_key(|event| (event.seq, event.session_id.clone()));

        let mut live_plan = plan;
        let mut live_commands = commands;
        for event in &events {
            match &event.event {
                AgentStateEvent::Plan { entries } if event.seq > next_cursor => {
                    live_plan = Some(entries.clone());
                }
                AgentStateEvent::AvailableCommands { commands } if event.seq > next_cursor => {
                    live_commands = Some(commands.clone());
                }
                _ => {}
            }
        }
        if live_plan.is_none() {
            for event in &events {
                if let AgentStateEvent::Plan { entries } = &event.event {
                    live_plan = Some(entries.clone());
                }
            }
        }
        if live_commands.is_none() {
            for event in &events {
                if let AgentStateEvent::AvailableCommands { commands } = &event.event {
                    live_commands = Some(commands.clone());
                }
            }
        }
        self.plan = live_plan;
        self.commands = live_commands;

        self.events = events;
        self.trim_events();
        self.events_revision = self.events_revision.wrapping_add(1);
        if latest_session.is_some() {
            self.events_session = latest_session;
        }
        self.events_cursor = self.events_cursor.max(highest);
        self.events_loading = false;
        self.events_error = None;
        self.events_loaded = true;
        self.events_ignored = ignored;
    }

    /// Fold a backfill failure in; loaded events are kept.
    pub fn fail_events(&mut self, message: String) {
        self.events_loading = false;
        self.events_error = Some(message);
    }

    /// Append one live event, deduplicating by `(session_id, seq)`. Returns
    /// whether it was new; a new event while the viewport is pinned off the tail
    /// increments [`Conversation::new_events`].
    pub fn push_event(&mut self, push: AgentEventPush) -> bool {
        if push.seq <= self.events_floor {
            return false;
        }
        if self
            .events
            .iter()
            .any(|event| event.session_id == push.session_id && event.seq == push.seq)
        {
            return false;
        }
        self.events_cursor = self.events_cursor.max(push.seq);
        if self.events_session.is_none() {
            self.events_session = Some(push.session_id.clone());
        }
        match &push.event {
            AgentStateEvent::Plan { entries } => self.plan = Some(entries.clone()),
            AgentStateEvent::AvailableCommands { commands } => {
                self.commands = Some(commands.clone())
            }
            _ => {}
        }
        self.events.push(push);
        self.events.sort_by_key(|event| event.seq);
        self.trim_events();
        self.events_revision = self.events_revision.wrapping_add(1);
        debug_assert!(self
            .events
            .windows(2)
            .all(|pair| pair[0].seq <= pair[1].seq));
        if !self.follow {
            self.new_events = self.new_events.saturating_add(1);
        }
        true
    }

    /// Keep the retained feed bounded. The oldest frames go first.
    fn trim_events(&mut self) {
        let excess = self.events.len().saturating_sub(MAX_RETAINED_EVENTS);
        if excess > 0 {
            self.events.drain(..excess);
        }
    }

    /// The grouped event feed.
    pub fn feed(&self) -> Vec<FeedItem> {
        build_feed(&self.events)
    }

    /// Whether a live push belongs to this conversation.
    pub fn accepts_event(&self, event: &AgentEventPush, sessions: &[AgentSessionInfo]) -> bool {
        match &self.target {
            ConversationTarget::Task(task_id) => {
                if event
                    .task_id
                    .as_deref()
                    .and_then(|id| Uuid::parse_str(id).ok())
                    == Some(*task_id)
                {
                    return true;
                }
                if self.events_session.as_deref() == Some(event.session_id.as_str()) {
                    return true;
                }
                sessions.iter().any(|session| {
                    session.id == event.session_id
                        && session
                            .task_id
                            .as_deref()
                            .and_then(|id| Uuid::parse_str(id).ok())
                            == Some(*task_id)
                })
            }
            ConversationTarget::Session(session_id) => {
                !session_id.is_empty()
                    && (event.session_id == *session_id
                        || self.events_session.as_deref() == Some(session_id.as_str()))
            }
        }
    }

    /// The latest unresolved structured prompt in the event feed.
    fn latest_unresolved_request(&self) -> Option<(&str, &favetto_wire::model::InputRequest)> {
        let mut pending = None;
        for push in &self.events {
            match &push.event {
                AgentStateEvent::InputRequested { request } => {
                    pending = Some((push.session_id.as_str(), request));
                }
                AgentStateEvent::InputResolved { id } => {
                    if pending.is_some_and(|(_, request)| request.id == *id) {
                        pending = None;
                    }
                }
                AgentStateEvent::Idle { .. } => pending = None,
                _ => {}
            }
        }
        pending
    }

    /// The prompt the permission banner should show, if any.
    ///
    /// The live `agents.list` session is authoritative; the last unresolved
    /// `InputRequested` event is the fallback. The request id is always used
    /// verbatim, never guessed.
    pub fn pending_request(&self, sessions: &[AgentSessionInfo]) -> Option<PendingRequest> {
        if self.terminal {
            return None;
        }
        if let Some(session) = sessions
            .iter()
            .find(|session| self.target.matches_session(session))
        {
            if let Some(reason) = &session.awaiting_input {
                let answered = reason.request_id.is_some()
                    && self.answered_request.as_deref() == reason.request_id.as_deref();
                if !answered {
                    return Some(PendingRequest {
                        session_id: session.id.clone(),
                        request_id: reason.request_id.clone(),
                        kind: reason.kind,
                        message: reason.message.clone(),
                        options: reason.options.clone(),
                        allow_always: reason.allow_always,
                    });
                }
                return None;
            }
        }

        if let Some((session_id, request)) = self.latest_unresolved_request() {
            let answered = self.answered_request.as_deref() == Some(request.id.as_str());
            if !answered {
                return Some(PendingRequest {
                    session_id: session_id.to_string(),
                    request_id: Some(request.id.clone()),
                    kind: request.kind,
                    message: request.message.clone(),
                    options: request.options.clone(),
                    allow_always: request.allow_always,
                });
            }
            return None;
        }

        if self.awaiting_input {
            return Some(PendingRequest {
                session_id: self.events_session.clone().unwrap_or_default(),
                request_id: None,
                kind: AwaitingInputKind::Other,
                message: "The agent is waiting for input, but its prompt is not available through the structured channel yet.".to_string(),
                options: Vec::new(),
                allow_always: false,
            });
        }

        None
    }

    /// The latest unresolved ACP form request in the feed.
    fn latest_unresolved_elicitation(
        &self,
    ) -> Option<(&str, &favetto_wire::elicitation::ElicitationRequest)> {
        let mut pending = None;
        for push in &self.events {
            match &push.event {
                AgentStateEvent::ElicitationRequested { request } => {
                    pending = Some((push.session_id.as_str(), request));
                }
                AgentStateEvent::ElicitationResolved { id, .. } => {
                    if pending.is_some_and(|(_, request)| request.id == *id) {
                        pending = None;
                    }
                }
                AgentStateEvent::Idle { .. } => pending = None,
                _ => {}
            }
        }
        pending
    }

    /// The pending form, preferring the live roster's `awaiting_input` and
    /// falling back to the feed.
    pub fn pending_elicitation(&self, sessions: &[AgentSessionInfo]) -> Option<PendingElicitation> {
        if self.terminal {
            return None;
        }
        if let Some(session) = sessions
            .iter()
            .find(|session| self.target.matches_session(session))
        {
            if let Some(reason) = &session.awaiting_input {
                if reason.kind == AwaitingInputKind::Elicitation {
                    let form = reason.form.clone()?;
                    let request_id = reason.request_id.clone()?;
                    if self.answered_elicitation.as_deref() == Some(request_id.as_str()) {
                        return None;
                    }
                    return Some(PendingElicitation {
                        session_id: session.id.clone(),
                        request: favetto_wire::elicitation::ElicitationRequest {
                            id: request_id,
                            agent: session.agent.clone(),
                            tool_call_id: None,
                            message: reason.message.clone(),
                            form,
                        },
                    });
                }
            }
        }
        let (session_id, request) = self.latest_unresolved_elicitation()?;
        if self.answered_elicitation.as_deref() == Some(request.id.as_str()) {
            return None;
        }
        Some(PendingElicitation {
            session_id: session_id.to_string(),
            request: request.clone(),
        })
    }

    /// Create the editable form once a pending request appears, and clear it
    /// only when the same request resolves.
    pub fn sync_elicitation(&mut self, sessions: &[AgentSessionInfo]) {
        let pending = self.pending_elicitation(sessions);
        match pending {
            Some(pending) => {
                let same = self
                    .elicitation
                    .as_ref()
                    .is_some_and(|form| form.request.id == pending.request.id);
                if !same {
                    self.elicitation = Some(crate::elicitation::ElicitationFormState::new(
                        pending.session_id,
                        pending.request,
                    ));
                }
            }
            None => self.elicitation = None,
        }
    }

    /// The request id with an in-flight reply, when it matches `request_id`.
    pub fn reply_in_flight(&self, request_id: Option<&str>) -> bool {
        match (request_id, self.reply_pending.as_deref()) {
            (Some(request_id), Some(pending)) => request_id == pending,
            (None, pending) => pending.is_some(),
            _ => false,
        }
    }

    /// The current plan, falling back to the last matching event.
    pub fn plan(&self) -> Vec<AgentPlanEntry> {
        if let Some(plan) = &self.plan {
            return plan.clone();
        }
        self.events
            .iter()
            .rev()
            .find_map(|push| match &push.event {
                AgentStateEvent::Plan { entries } => Some(entries.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// The current advertised commands, with the same event fallback as
    /// [`Self::plan`].
    pub fn commands(&self) -> Vec<AgentCommand> {
        if let Some(commands) = &self.commands {
            return commands.clone();
        }
        self.events
            .iter()
            .rev()
            .find_map(|push| match &push.event {
                AgentStateEvent::AvailableCommands { commands } => Some(commands.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// The `/prefix` currently being completed, only for a bare first token.
    pub fn command_query(&self) -> Option<&str> {
        command_query(self.prompt.value())
    }

    /// Advertised commands matching the current query, most relevant first.
    pub fn command_matches(&self) -> Vec<&AgentCommand> {
        if self.command_query().is_none() {
            return Vec::new();
        }
        let Some(commands) = &self.commands else {
            return Vec::new();
        };
        matching_commands(commands, self.prompt.value())
            .into_iter()
            .map(|(_, command)| command)
            .collect()
    }

    /// Move the highlighted completion by `delta`, clamped to the matches.
    pub fn move_command(&mut self, delta: isize) -> bool {
        let count = self.command_matches().len();
        if count == 0 {
            self.prompt_selected = None;
            return false;
        }
        let current = self.prompt_selected.unwrap_or(0).min(count - 1);
        let next = (current as isize + delta).clamp(0, count as isize - 1);
        self.prompt_selected = Some(next as usize);
        true
    }

    /// Accept the highlighted (or first) command into the draft. Only an
    /// explicit accept rewrites the prompt; unknown text is never altered.
    pub fn complete_command(&mut self) -> bool {
        let name = self
            .command_matches()
            .get(self.prompt_selected.unwrap_or(0))
            .map(|command| command.name.clone());
        let Some(name) = name else {
            return false;
        };
        self.prompt.set_value(format!("/{name} "));
        self.prompt_selected = None;
        true
    }

    /// The exact matched command's input hint, when present.
    #[allow(dead_code)] // Kept on the pure shared model for parity with the TUI.
    pub fn command_hint(&self) -> Option<&str> {
        let rest = self.prompt.value().strip_prefix('/')?;
        let name = rest.split_whitespace().next().unwrap_or_default();
        self.commands
            .as_ref()?
            .iter()
            .find(|command| command.name == name)
            .and_then(|command| command.input_hint.as_deref())
    }

    /// Clear the prompt without sending.
    pub fn clear_prompt(&mut self) {
        self.prompt.set_value(String::new());
        self.prompt_selected = None;
        self.prompt_error = None;
    }

    /// Whether a prompt/options operation addresses this conversation.
    pub fn is_session(&self, session_id: &str) -> bool {
        self.target.is_session(session_id) || self.events_session.as_deref() == Some(session_id)
    }

    /// Record an accepted ACP form reply and clear the matching form.
    pub fn elicitation_done(&mut self, request_id: &str) {
        self.elicitation_pending = None;
        if self
            .elicitation
            .as_ref()
            .map(|form| form.request.id.as_str())
            == Some(request_id)
        {
            self.elicitation = None;
        }
        self.answered_elicitation = Some(request_id.to_string());
    }

    /// Record the chosen reply before the RPC lands.
    pub fn begin_reply(&mut self, request_id: &str) {
        self.reply_pending = Some(request_id.to_string());
    }

    /// Cancel a pending reply after a transport failure.
    pub fn fail_reply(&mut self) {
        self.reply_pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use favetto_wire::model::{AgentSessionInfo, InputRequest, MessageRole};

    fn push(seq: u64, session: &str, event: AgentStateEvent) -> AgentEventPush {
        AgentEventPush {
            session_id: session.to_string(),
            task_id: None,
            seq,
            event,
        }
    }

    fn session(id: &str, task_id: Option<Uuid>) -> AgentSessionInfo {
        AgentSessionInfo {
            id: id.to_string(),
            agent: "opencode".to_string(),
            backend: Default::default(),
            task_id: task_id.map(|id| id.to_string()),
            running: true,
            headless: true,
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

    #[test]
    fn a_task_conversation_accepts_its_task_and_session_events() {
        let task_id = Uuid::from_u128(7);
        let conversation = Conversation::for_task(task_id);
        let mut event = push(1, "ses_1", AgentStateEvent::TurnStarted);
        event.task_id = Some(task_id.to_string());
        assert!(conversation.accepts_event(&event, &[]));
        assert!(conversation.accepts_event(
            &push(2, "ses_1", AgentStateEvent::TurnStarted),
            &[session("ses_1", Some(task_id))]
        ));
        assert!(!conversation.accepts_event(&push(3, "other", AgentStateEvent::TurnStarted), &[]));
    }

    #[test]
    fn a_session_conversation_accepts_only_its_own_session() {
        let conversation = Conversation::for_session("ses_a");
        assert!(conversation.accepts_event(&push(1, "ses_a", AgentStateEvent::TurnStarted), &[]));
        assert!(!conversation.accepts_event(&push(2, "ses_b", AgentStateEvent::TurnStarted), &[]));
        assert!(!Conversation::for_session("")
            .accepts_event(&push(3, "", AgentStateEvent::TurnStarted), &[]));
    }

    #[test]
    fn a_session_conversation_correlates_prompts_without_a_task() {
        let mut conversation = Conversation::for_session("ses_a");
        conversation.push_event(push(
            1,
            "ses_a",
            AgentStateEvent::InputRequested {
                request: InputRequest {
                    id: "perm_1".to_string(),
                    kind: AwaitingInputKind::Permission,
                    message: "Allow?".to_string(),
                    options: vec!["yes".to_string()],
                    allow_always: true,
                    form: None,
                },
            },
        ));
        let pending = conversation.pending_request(&[]).expect("pending");
        assert_eq!(pending.session_id, "ses_a");
        assert_eq!(pending.request_id.as_deref(), Some("perm_1"));
        conversation.answer_request_for_test("perm_1");
        assert!(conversation.pending_request(&[]).is_none());
    }

    impl Conversation {
        fn answer_request_for_test(&mut self, id: &str) {
            self.answered_request = Some(id.to_string());
        }
    }

    #[test]
    fn session_events_update_plan_and_commands_live() {
        let mut conversation = Conversation::for_session("ses_a");
        conversation.push_event(push(
            1,
            "ses_a",
            AgentStateEvent::Plan {
                entries: vec![AgentPlanEntry {
                    content: "inspect".to_string(),
                    ..Default::default()
                }],
            },
        ));
        conversation.push_event(push(
            2,
            "ses_a",
            AgentStateEvent::AvailableCommands {
                commands: vec![AgentCommand {
                    name: "web".to_string(),
                    description: "Search".to_string(),
                    input_hint: None,
                }],
            },
        ));
        assert_eq!(conversation.plan()[0].content, "inspect");
        assert_eq!(conversation.commands()[0].name, "web");
        assert_eq!(conversation.events_cursor, 2);
    }

    #[test]
    fn slash_completion_uses_the_shared_ranking() {
        let mut conversation = Conversation::for_session("ses_a");
        conversation.commands = Some(vec![AgentCommand {
            name: "web".to_string(),
            description: "Search".to_string(),
            input_hint: Some("query".to_string()),
        }]);
        conversation.prompt.set_value("/w".to_string());
        assert!(conversation.complete_command());
        assert_eq!(conversation.prompt, "/web ");
        assert_eq!(conversation.command_hint(), Some("query"));
    }

    #[test]
    fn the_composer_is_read_only_without_a_live_session_or_after_the_run_ends() {
        let mut conversation = Conversation::for_task(Uuid::from_u128(7));
        assert!(conversation.prompt_enabled(true));
        assert!(!conversation.prompt_enabled(false));

        conversation.mark_status(true, false);
        assert!(
            !conversation.prompt_enabled(true),
            "a finished run cannot accept a prompt, even with a restored session"
        );

        let mut session = Conversation::for_session("ses_a");
        assert!(session.prompt_enabled(true));
        session.mark_status(true, false);
        assert!(!session.prompt_enabled(true));
    }

    #[test]
    fn a_new_attempt_releases_the_composer_but_keeps_the_draft() {
        let mut conversation = Conversation::for_task(Uuid::from_u128(7));
        conversation.prompt.set_value("draft".to_string());
        conversation.prompt_focused = true;
        conversation.reset_for_new_attempt();
        assert!(!conversation.prompt_focused);
        assert!(conversation.prompt.is_empty());
    }

    #[test]
    fn a_new_attempt_clears_the_feed_and_draft() {
        let mut conversation = Conversation::for_task(Uuid::from_u128(7));
        conversation.push_event(push(
            1,
            "ses_1",
            AgentStateEvent::TextDelta {
                role: MessageRole::Assistant,
                text: "old".to_string(),
            },
        ));
        conversation.prompt.set_value("/x".to_string());
        conversation.reset_for_new_attempt();
        assert!(conversation.events.is_empty());
        assert!(conversation.prompt.is_empty());
        assert!(conversation.plan.is_none());
    }
}
