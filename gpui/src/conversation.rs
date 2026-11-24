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
    AgentActivity, AgentCommand, AgentPlanEntry, AgentSessionInfo, AgentStateEvent,
    AwaitingInputKind, ContentRole, InputRequest, MessageRole, RunSummary, SessionConfigChoices,
    SessionConfigKind, SessionConfigOption, SessionModes,
};
use favetto_wire::rpc::AgentEventPush;
use uuid::Uuid;

use crate::detail::{
    build_feed_with_outgoing, command_query, matching_commands, summary_feed, FeedItem,
    PendingElicitation, PendingRequest, MAX_RETAINED_EVENTS,
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

    /// A stable key for animation ids: the run id or the session id, never the
    /// row index, so a re-render keeps its phase.
    pub fn animation_key(&self) -> String {
        match self {
            ConversationTarget::Task(task_id) => task_id.to_string(),
            ConversationTarget::Session(session_id) => format!("session-{session_id}"),
        }
    }
}

/// A locally echoed user prompt, shown immediately while the daemon accepts and
/// the agent starts working. The row is transient: it is dropped once the agent
/// echoes the same prompt back through the structured feed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutgoingState {
    /// `agents.prompt` is in flight; the composer keeps the draft until accepted.
    Sending,
    /// The daemon accepted the prompt.
    Sent,
}

/// One locally echoed prompt and where it sits in the feed timeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutgoingMessage {
    /// Monotonic per-conversation id, used for stable animation/list keys.
    pub id: u64,
    /// The prompt text exactly as sent.
    pub text: String,
    /// Whether the daemon has accepted the prompt.
    pub state: OutgoingState,
    /// The highest feed seq at send time. The row renders before any later
    /// event, so a response always appears below its prompt.
    pub anchor: u64,
}

/// The tone of a live activity row. The label always carries the meaning; the
/// tone only picks the semantic color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityTone {
    /// The agent is working.
    Active,
    /// The agent is blocked on the user.
    Waiting,
}

/// A derived, always-visible "the agent is working" line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivityIndicator {
    /// The text label, for example "Thinking" or "Running read".
    pub label: String,
    /// The semantic color.
    pub tone: ActivityTone,
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
    /// The session id whose `agents.set_option` / `agents.set_mode` is in
    /// flight. A second selector click is ignored until the daemon snapshot
    /// lands, so a stale snapshot can never re-send the same target.
    pub session_option_pending: Option<String>,
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
    /// Locally echoed prompts, newest last.
    pub outgoing: Vec<OutgoingMessage>,
    /// The session the in-flight prompt was sent to, so its reply is routed even
    /// before the feed has recorded a session id.
    outgoing_session: Option<String>,
    /// The next [`OutgoingMessage::id`].
    next_outgoing_id: u64,
    /// A prompt was accepted but the daemon has not reported activity yet, so
    /// the client still paints "Thinking" rather than a frozen panel.
    pub turn_pending: bool,
    /// The structured activity the daemon last reported for this session.
    pub activity: Option<AgentActivity>,
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
            session_option_pending: None,
            answered_request: None,
            plan: None,
            commands: None,
            prompt: TextInput::default(),
            prompt_selected: None,
            prompt_sending: false,
            prompt_focused: false,
            prompt_error: None,
            outgoing: Vec::new(),
            outgoing_session: None,
            next_outgoing_id: 0,
            turn_pending: false,
            activity: None,
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
        if terminal {
            // A terminal run/session must never keep a spinner alive.
            self.activity = None;
            self.turn_pending = false;
            self.prompt_sending = false;
            for message in &mut self.outgoing {
                if message.state == OutgoingState::Sending {
                    message.state = OutgoingState::Sent;
                }
            }
        }
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
        self.session_option_pending = None;
        self.plan = None;
        self.commands = None;
        self.prompt.set_value(String::new());
        self.prompt_selected = None;
        self.prompt_sending = false;
        self.prompt_focused = false;
        self.prompt_error = None;
        self.outgoing.clear();
        self.outgoing_session = None;
        self.turn_pending = false;
        self.activity = None;
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
        self.session_option_pending = None;
        self.outgoing.clear();
        self.outgoing_session = None;
        self.turn_pending = false;
        self.activity = None;
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
        // A backfilled feed carries the daemon's history, so the newest
        // activity-bearing frame is the honest current activity.
        let mut activity = None;
        for event in &self.events {
            note_activity(&mut activity, &event.event);
        }
        self.activity = activity;
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
        let mut activity = self.activity.clone();
        note_activity(&mut activity, &push.event);
        self.activity = activity;
        // Any real agent signal supersedes the optimistic "Thinking" placeholder.
        if !matches!(
            &push.event,
            AgentStateEvent::Plan { .. }
                | AgentStateEvent::AvailableCommands { .. }
                | AgentStateEvent::ConfigOptions { .. }
                | AgentStateEvent::Modes { .. }
                | AgentStateEvent::CurrentMode { .. }
                | AgentStateEvent::Session { .. }
                | AgentStateEvent::Title { .. }
                | AgentStateEvent::Auth { .. }
                | AgentStateEvent::Usage { .. }
        ) {
            self.turn_pending = false;
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

    /// The grouped event feed, with locally echoed prompts interleaved and
    /// deduplicated against an agent-emitted user echo.
    pub fn feed(&self) -> Vec<FeedItem> {
        let mut items = build_feed_with_outgoing(&self.events, &self.outgoing);
        if !self.outgoing.is_empty() {
            let echoed: Vec<String> = items
                .iter()
                .filter_map(|item| match item {
                    FeedItem::Message {
                        role: MessageRole::User,
                        text,
                    } => Some(normalize_prompt(text)),
                    _ => None,
                })
                .collect();
            if !echoed.is_empty() {
                items.retain(|item| match item {
                    FeedItem::OutgoingUser { text, .. } => {
                        let local = normalize_prompt(text);
                        !echoed.iter().any(|echo| prompts_match(&local, echo))
                    }
                    _ => true,
                });
            }
        }
        items
    }

    /// The durable summary feed, keeping the locally echoed prompts so a
    /// terminal run does not present the summary as an assistant-only
    /// conversation.
    pub fn summary_feed(&self, summary: &RunSummary) -> Vec<FeedItem> {
        let mut items: Vec<FeedItem> = self
            .outgoing
            .iter()
            .map(|message| FeedItem::OutgoingUser {
                text: message.text.clone(),
                state: message.state,
            })
            .collect();
        items.extend(summary_feed(summary));
        items
    }

    /// Echo a prompt locally the moment Enter is pressed.
    pub fn queue_prompt(&mut self, text: String, session_id: impl Into<String>) {
        let anchor = self.events_cursor;
        let id = self.next_outgoing_id;
        self.next_outgoing_id = self.next_outgoing_id.wrapping_add(1);
        self.outgoing.push(OutgoingMessage {
            id,
            text,
            state: OutgoingState::Sending,
            anchor,
        });
        self.outgoing_session = Some(session_id.into());
        self.prompt_sending = true;
        self.prompt_error = None;
        self.events_revision = self.events_revision.wrapping_add(1);
        if !self.follow {
            self.new_events = self.new_events.saturating_add(1);
        }
    }

    /// Whether this conversation owns the reply for `session_id`: either it sent
    /// the in-flight prompt, or the session is the one it is showing.
    pub fn owns_prompt_reply(&self, session_id: &str, sessions: &[AgentSessionInfo]) -> bool {
        self.outgoing_session.as_deref() == Some(session_id)
            || self.owns_session(session_id, sessions)
    }

    /// The daemon accepted the prompt: mark the row sent, clear the draft only
    /// now, and start the optimistic "Thinking" phase until real activity lands.
    pub fn accept_prompt(&mut self) {
        if let Some(message) = self
            .outgoing
            .iter_mut()
            .rev()
            .find(|message| message.state == OutgoingState::Sending)
        {
            message.state = OutgoingState::Sent;
        }
        self.outgoing_session = None;
        self.prompt_sending = false;
        self.turn_pending = true;
        self.clear_prompt();
        self.events_revision = self.events_revision.wrapping_add(1);
    }

    /// The send failed: keep the draft recoverable and drop the local row.
    pub fn fail_prompt(&mut self) {
        if let Some(index) = self
            .outgoing
            .iter()
            .rposition(|message| message.state == OutgoingState::Sending)
        {
            self.outgoing.remove(index);
        }
        self.outgoing_session = None;
        self.prompt_sending = false;
        self.turn_pending = false;
        self.events_revision = self.events_revision.wrapping_add(1);
    }

    /// Apply the activity from an `agent.state` push. `None` leaves the last
    /// known value alone so a sparse push cannot blank a live spinner.
    pub fn apply_activity(&mut self, activity: Option<AgentActivity>) {
        if let Some(activity) = activity {
            self.turn_pending = false;
            self.activity = Some(activity);
        }
    }

    /// Reconcile activity with the authoritative `agents.list` roster.
    ///
    /// A snapshot can lag the push it follows, so a stale `Idle` never cancels
    /// the optimistic "Thinking" of a prompt the daemon just accepted; the
    /// per-event `agent.state` push remains authoritative for that.
    pub fn sync_activity(&mut self, sessions: &[AgentSessionInfo]) {
        let Some(activity) = sessions
            .iter()
            .find(|session| self.target.matches_session(session))
            .and_then(|session| session.activity.clone())
        else {
            return;
        };
        if self.turn_pending && matches!(activity, AgentActivity::Idle) {
            return;
        }
        self.apply_activity(Some(activity));
    }

    /// The always-visible activity label, or `None` when the agent is idle or
    /// the conversation is terminal.
    pub fn activity_indicator(&self) -> Option<ActivityIndicator> {
        if self.terminal {
            return None;
        }
        if self.prompt_sending {
            return Some(ActivityIndicator {
                label: "Sending…".to_string(),
                tone: ActivityTone::Active,
            });
        }
        let active = |label: String| ActivityIndicator {
            label,
            tone: ActivityTone::Active,
        };
        match self.activity.as_ref() {
            Some(AgentActivity::Starting) => return Some(active("Starting…".to_string())),
            Some(AgentActivity::Thinking) => return Some(active("Thinking".to_string())),
            Some(AgentActivity::Responding) => return Some(active("Responding".to_string())),
            Some(AgentActivity::Tool { name, .. }) => {
                return Some(active(format!("Running {name}")))
            }
            Some(AgentActivity::Waiting { .. }) => {
                return Some(ActivityIndicator {
                    label: "Waiting for you".to_string(),
                    tone: ActivityTone::Waiting,
                });
            }
            Some(AgentActivity::Idle) => {
                if !self.turn_pending {
                    return None;
                }
            }
            Some(AgentActivity::Exited { .. }) => return None,
            None => {}
        }
        if self.turn_pending {
            return Some(active("Thinking".to_string()));
        }
        self.awaiting_input.then(|| ActivityIndicator {
            label: "Waiting for you".to_string(),
            tone: ActivityTone::Waiting,
        })
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

    /// Whether `session_id` is the session behind this conversation. Unlike
    /// [`Self::is_session`], a task target matches its live session too, so a
    /// per-session push can be applied without a full roster refresh.
    pub fn owns_session(&self, session_id: &str, sessions: &[AgentSessionInfo]) -> bool {
        self.is_session(session_id)
            || sessions
                .iter()
                .any(|session| session.id == session_id && self.target.matches_session(session))
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

    /// Whether an `agents.set_option` / `agents.set_mode` is in flight.
    pub fn session_option_change_pending(&self) -> bool {
        self.session_option_pending.is_some()
    }

    /// Claim the session-option round trip. Returns `false` when one is already
    /// outstanding, so the view cannot queue a second target against a stale
    /// snapshot.
    pub fn begin_session_option_change(&mut self, session_id: &str) -> bool {
        if self.session_option_pending.is_some() {
            return false;
        }
        self.session_option_pending = Some(session_id.to_string());
        true
    }

    /// Release the round trip when its `AgentOptionsChanged` update lands.
    pub fn finish_session_option_change(&mut self, session_id: &str) -> bool {
        if self.session_option_pending.as_deref() == Some(session_id) {
            self.session_option_pending = None;
            true
        } else {
            false
        }
    }
}

/// Which advertised session control an open select list edits.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SessionSelectTarget {
    /// The session's `SessionModes` snapshot (`agents.set_mode`).
    Modes,
    /// One `SessionConfigKind::Select` option (`agents.set_option`).
    Option(String),
}

/// One advertised value in an open session select list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSelectRow {
    /// The exact value id (or mode id) sent by the set RPC.
    pub value: String,
    /// Human-readable label.
    pub name: String,
    /// Optional agent-provided description.
    pub description: Option<String>,
    /// Group heading rendered before this row, when the agent grouped choices.
    pub group: Option<String>,
    /// Whether this is the value the session currently reports.
    pub selected: bool,
}

/// A selector flattened into the list of values the agent advertises.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSelect {
    /// The control the chosen row is applied to.
    pub target: SessionSelectTarget,
    /// The selector label (`mode`, `model`, `thought_level`, …).
    pub label: String,
    /// The raw current value id.
    pub current_value: String,
    /// The current value's advertised name, or its raw id when it is missing.
    pub current_name: String,
    /// Every advertised value, in the agent's order.
    pub rows: Vec<SessionSelectRow>,
}

impl SessionSelect {
    /// Flatten `modes`, or `None` when there is nothing to choose.
    pub fn modes(modes: &SessionModes) -> Option<Self> {
        if modes.available_modes.len() < 2 {
            return None;
        }
        let current_value = modes.current_mode_id.clone();
        let rows: Vec<SessionSelectRow> = modes
            .available_modes
            .iter()
            .map(|mode| SessionSelectRow {
                value: mode.id.clone(),
                name: mode.name.clone(),
                description: mode.description.clone(),
                group: None,
                selected: mode.id == current_value,
            })
            .collect();
        let current_name = rows
            .iter()
            .find(|row| row.value == current_value)
            .map(|row| row.name.clone())
            .unwrap_or_else(|| current_value.clone());
        Some(Self {
            target: SessionSelectTarget::Modes,
            label: "mode".to_string(),
            current_value,
            current_name,
            rows,
        })
    }

    /// Flatten a `Select` option, or `None` for booleans, unsupported options
    /// and single-choice lists (those stay read-only pills).
    pub fn option(option: &SessionConfigOption) -> Option<Self> {
        let SessionConfigKind::Select {
            current_value,
            choices,
        } = &option.kind
        else {
            return None;
        };
        if choices.all().len() < 2 {
            return None;
        }
        let rows = select_rows(choices, current_value);
        let current_name = rows
            .iter()
            .find(|row| row.value == *current_value)
            .map(|row| row.name.clone())
            .unwrap_or_else(|| current_value.clone());
        Some(Self {
            target: SessionSelectTarget::Option(option.id.clone()),
            label: option
                .category
                .clone()
                .unwrap_or_else(|| option.name.clone()),
            current_value: current_value.clone(),
            current_name,
            rows,
        })
    }

    /// Whether the agent still advertises the current value. A stale id stays
    /// visible as its raw value but is never marked as a list row.
    pub fn current_is_advertised(&self) -> bool {
        self.rows.iter().any(|row| row.value == self.current_value)
    }
}

fn select_rows(choices: &SessionConfigChoices, current: &str) -> Vec<SessionSelectRow> {
    let mut rows = Vec::new();
    match choices {
        SessionConfigChoices::Ungrouped { options } => {
            for choice in options {
                rows.push(SessionSelectRow {
                    value: choice.value.clone(),
                    name: choice.name.clone(),
                    description: choice.description.clone(),
                    group: None,
                    selected: choice.value == current,
                });
            }
        }
        SessionConfigChoices::Grouped { groups } => {
            for group in groups {
                for choice in &group.options {
                    rows.push(SessionSelectRow {
                        value: choice.value.clone(),
                        name: choice.name.clone(),
                        description: choice.description.clone(),
                        group: (!group.name.is_empty()).then(|| group.name.clone()),
                        selected: choice.value == current,
                    });
                }
            }
        }
    }
    rows
}

/// One open select list and its keyboard highlight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenSessionSelect {
    /// The flattened values and the control they apply to.
    pub select: SessionSelect,
    /// Index into [`SessionSelect::rows`].
    pub highlighted: usize,
}

/// The per-conversation open/closed and keyboard state for a session selector.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionSelectState {
    open: Option<OpenSessionSelect>,
}

impl SessionSelectState {
    /// Whether any select list is open.
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Whether the open list edits `target`.
    pub fn is_open_for(&self, target: &SessionSelectTarget) -> bool {
        self.open
            .as_ref()
            .is_some_and(|open| &open.select.target == target)
    }

    /// The open list, if any.
    pub fn select(&self) -> Option<&SessionSelect> {
        self.open.as_ref().map(|open| &open.select)
    }

    /// The target of the open list, if any.
    pub fn target(&self) -> Option<&SessionSelectTarget> {
        self.open.as_ref().map(|open| &open.select.target)
    }

    /// Open `select`, highlighting its current row.
    pub fn open(&mut self, select: SessionSelect) {
        let highlighted = select.rows.iter().position(|row| row.selected).unwrap_or(0);
        self.open = Some(OpenSessionSelect {
            select,
            highlighted,
        });
    }

    /// Close the list without changing anything.
    pub fn close(&mut self) {
        self.open = None;
    }

    /// Replace the open list's rows with a fresh snapshot of the same target,
    /// keeping the highlight on the same value when it is still advertised.
    pub fn refresh(&mut self, select: SessionSelect) {
        let Some(open) = self.open.as_mut() else {
            return;
        };
        if open.select.target != select.target {
            return;
        }
        let highlighted_value = open
            .select
            .rows
            .get(open.highlighted)
            .map(|row| row.value.clone());
        open.select = select;
        open.highlighted = highlighted_value
            .and_then(|value| open.select.rows.iter().position(|row| row.value == value))
            .unwrap_or(0)
            .min(open.select.rows.len().saturating_sub(1));
    }

    /// The highlighted row index.
    pub fn highlighted(&self) -> Option<usize> {
        self.open.as_ref().map(|open| open.highlighted)
    }

    /// The highlighted row.
    pub fn highlighted_row(&self) -> Option<&SessionSelectRow> {
        let open = self.open.as_ref()?;
        open.select.rows.get(open.highlighted)
    }

    /// Move the highlight by `delta`, clamped to the advertised rows.
    pub fn move_highlight(&mut self, delta: isize) -> bool {
        let Some(open) = self.open.as_mut() else {
            return false;
        };
        let count = open.select.rows.len();
        if count == 0 {
            return false;
        }
        let next = (open.highlighted as isize + delta).clamp(0, count as isize - 1);
        let next = next as usize;
        if next == open.highlighted {
            return false;
        }
        open.highlighted = next;
        true
    }

    /// Highlight the first advertised row.
    pub fn highlight_first(&mut self) -> bool {
        let Some(open) = self.open.as_mut() else {
            return false;
        };
        if open.highlighted == 0 || open.select.rows.is_empty() {
            return false;
        }
        open.highlighted = 0;
        true
    }

    /// Highlight the last advertised row.
    pub fn highlight_last(&mut self) -> bool {
        let Some(open) = self.open.as_mut() else {
            return false;
        };
        let last = open.select.rows.len().saturating_sub(1);
        if open.select.rows.is_empty() || open.highlighted == last {
            return false;
        }
        open.highlighted = last;
        true
    }

    /// The target and exact value of the highlighted row, ready to send.
    pub fn confirm(&self) -> Option<(SessionSelectTarget, String)> {
        let row = self.highlighted_row()?;
        Some((self.target()?.clone(), row.value.clone()))
    }
}

/// Fold one structured event into the derived activity label.
fn note_activity(activity: &mut Option<AgentActivity>, event: &AgentStateEvent) {
    let next = match event {
        AgentStateEvent::TurnStarted | AgentStateEvent::ReasoningDelta { .. } => {
            Some(AgentActivity::Thinking)
        }
        AgentStateEvent::TextDelta { .. } => Some(AgentActivity::Responding),
        AgentStateEvent::ContentBlock { role, .. } => Some(if *role == ContentRole::Thought {
            AgentActivity::Thinking
        } else {
            AgentActivity::Responding
        }),
        AgentStateEvent::ToolStarted { name, .. } => Some(AgentActivity::Tool {
            name: name.clone(),
            description: None,
        }),
        AgentStateEvent::ToolFinished { .. } | AgentStateEvent::ToolUpdated { .. } => {
            Some(AgentActivity::Thinking)
        }
        AgentStateEvent::InputRequested { request } => Some(AgentActivity::Waiting {
            request: request.clone(),
        }),
        AgentStateEvent::InputResolved { .. } => Some(AgentActivity::Thinking),
        AgentStateEvent::ElicitationRequested { request } => Some(AgentActivity::Waiting {
            request: InputRequest {
                id: request.id.clone(),
                kind: AwaitingInputKind::Elicitation,
                message: request.message.clone(),
                options: Vec::new(),
                allow_always: false,
                form: Some(request.form.clone()),
            },
        }),
        AgentStateEvent::ElicitationResolved { .. } => Some(AgentActivity::Thinking),
        AgentStateEvent::Idle { .. } => Some(AgentActivity::Idle),
        AgentStateEvent::Error { .. } => None,
        _ => return,
    };
    *activity = next;
}

/// Normalize a prompt for echo comparison: trimmed with collapsed whitespace.
fn normalize_prompt(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether an agent-emitted user message is the echo of a local prompt. A
/// streamed echo is a prefix/suffix of the full local text until it completes.
fn prompts_match(local: &str, echoed: &str) -> bool {
    if local == echoed {
        return true;
    }
    local.chars().count() >= 3
        && echoed.chars().count() >= 3
        && (local.contains(echoed) || echoed.contains(local))
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

    fn modes(current: &str) -> SessionModes {
        use favetto_wire::model::SessionMode;
        SessionModes {
            current_mode_id: current.to_string(),
            available_modes: vec![
                SessionMode {
                    id: "build".to_string(),
                    name: "Build".to_string(),
                    description: Some("Edit files".to_string()),
                },
                SessionMode {
                    id: "plan".to_string(),
                    name: "Plan".to_string(),
                    description: None,
                },
                SessionMode {
                    id: "review".to_string(),
                    name: "Review".to_string(),
                    description: None,
                },
            ],
        }
    }

    fn select_option(current: &str, choices: SessionConfigChoices) -> SessionConfigOption {
        SessionConfigOption {
            id: "model".to_string(),
            name: "Model".to_string(),
            description: None,
            category: Some("model".to_string()),
            kind: SessionConfigKind::Select {
                current_value: current.to_string(),
                choices,
            },
        }
    }

    fn ungrouped(values: &[(&str, &str)]) -> SessionConfigChoices {
        use favetto_wire::model::SessionConfigChoice;
        SessionConfigChoices::Ungrouped {
            options: values
                .iter()
                .map(|(value, name)| SessionConfigChoice {
                    value: value.to_string(),
                    name: name.to_string(),
                    description: None,
                })
                .collect(),
        }
    }

    #[test]
    fn the_mode_select_preserves_order_and_marks_the_current_mode() {
        let select = SessionSelect::modes(&modes("plan")).expect("three modes make a list");
        assert_eq!(select.target, SessionSelectTarget::Modes);
        assert_eq!(select.label, "mode");
        assert_eq!(select.current_name, "Plan");
        let values: Vec<&str> = select.rows.iter().map(|row| row.value.as_str()).collect();
        assert_eq!(values, ["build", "plan", "review"]);
        assert!(select.rows[1].selected);
        assert_eq!(select.rows[0].description.as_deref(), Some("Edit files"));
    }

    #[test]
    fn a_single_mode_or_single_choice_stays_read_only() {
        let mut one = modes("build");
        one.available_modes.truncate(1);
        assert!(SessionSelect::modes(&one).is_none());
        assert!(SessionSelect::option(&select_option("a", ungrouped(&[("a", "A")]))).is_none());
        let boolean = SessionConfigOption {
            id: "fast".to_string(),
            name: "Fast".to_string(),
            description: None,
            category: None,
            kind: SessionConfigKind::Boolean {
                current_value: true,
            },
        };
        assert!(SessionSelect::option(&boolean).is_none());
    }

    #[test]
    fn grouped_choices_keep_group_names_and_agent_order() {
        use favetto_wire::model::{SessionConfigChoice, SessionConfigGroup};
        let grouped = SessionConfigChoices::Grouped {
            groups: vec![
                SessionConfigGroup {
                    id: "frontier".to_string(),
                    name: "Frontier".to_string(),
                    options: vec![SessionConfigChoice {
                        value: "opus".to_string(),
                        name: "Opus".to_string(),
                        description: Some("Most capable".to_string()),
                    }],
                },
                SessionConfigGroup {
                    id: "fast".to_string(),
                    name: "Fast".to_string(),
                    options: vec![SessionConfigChoice {
                        value: "haiku".to_string(),
                        name: "Haiku".to_string(),
                        description: None,
                    }],
                },
            ],
        };
        let select = SessionSelect::option(&select_option("haiku", grouped)).expect("two choices");
        let rows: Vec<(&str, Option<&str>)> = select
            .rows
            .iter()
            .map(|row| (row.value.as_str(), row.group.as_deref()))
            .collect();
        assert_eq!(rows, [("opus", Some("Frontier")), ("haiku", Some("Fast"))]);
        assert!(select.rows[1].selected);
        assert_eq!(select.rows[0].description.as_deref(), Some("Most capable"));
    }

    #[test]
    fn a_stale_current_value_stays_visible_without_inventing_a_row() {
        let select = SessionSelect::option(&select_option(
            "retired",
            ungrouped(&[("a", "A"), ("b", "B")]),
        ))
        .expect("two advertised choices");
        assert_eq!(select.current_name, "retired");
        assert!(!select.current_is_advertised());
        assert!(select.rows.iter().all(|row| !row.selected));
    }

    #[test]
    fn the_select_state_starts_on_the_current_row_and_clamps_keyboard_moves() {
        let mut state = SessionSelectState::default();
        state.open(SessionSelect::modes(&modes("plan")).unwrap());
        assert_eq!(state.highlighted(), Some(1));
        assert_eq!(state.highlighted_row().unwrap().value, "plan");

        assert!(state.move_highlight(-1));
        assert_eq!(state.highlighted(), Some(0));
        assert!(!state.move_highlight(-1), "the first row is the floor");
        assert!(state.highlight_last());
        assert_eq!(state.highlighted(), Some(2));
        assert!(!state.move_highlight(1), "the last row is the ceiling");
        assert!(state.highlight_first());
        assert_eq!(
            state.confirm(),
            Some((SessionSelectTarget::Modes, "build".to_string()))
        );
    }

    #[test]
    fn a_select_click_is_ignored_while_a_set_is_in_flight() {
        let mut conversation = Conversation::for_session("ses_a");
        assert!(conversation.begin_session_option_change("ses_a"));
        assert!(conversation.session_option_change_pending());
        assert!(!conversation.begin_session_option_change("ses_a"));
        assert!(!conversation.finish_session_option_change("other"));
        assert!(conversation.finish_session_option_change("ses_a"));
        assert!(!conversation.session_option_change_pending());
    }

    #[test]
    fn refreshing_an_open_list_keeps_the_highlight_on_the_same_value() {
        let mut state = SessionSelectState::default();
        state.open(SessionSelect::modes(&modes("plan")).unwrap());
        assert_eq!(state.highlighted_row().unwrap().value, "plan");

        // The agent still advertises `plan`, but adds a mode in a new position.
        let mut fresh = modes("build");
        fresh.available_modes.insert(
            0,
            favetto_wire::model::SessionMode {
                id: "debug".to_string(),
                name: "Debug".to_string(),
                description: None,
            },
        );
        state.refresh(SessionSelect::modes(&fresh).unwrap());
        assert_eq!(state.highlighted_row().unwrap().value, "plan");
        assert_eq!(state.highlighted(), Some(2));
    }

    #[test]
    fn an_accepted_prompt_echoes_a_sent_user_row_and_starts_thinking() {
        let mut conversation = Conversation::for_session("ses_a");
        conversation.prompt.set_value("hello".to_string());
        conversation.queue_prompt("hello".to_string(), "ses_a");
        assert!(conversation.prompt_sending);
        assert_eq!(
            conversation.activity_indicator().map(|a| a.label),
            Some("Sending…".to_string())
        );

        conversation.accept_prompt();
        assert!(conversation.prompt.is_empty());
        assert!(!conversation.prompt_sending);
        assert_eq!(
            conversation.outgoing[0].state,
            OutgoingState::Sent,
            "the local row keeps its sent state"
        );
        assert_eq!(
            conversation.activity_indicator().map(|a| a.label),
            Some("Thinking".to_string()),
            "accepted prompts show immediate activity"
        );
        assert!(matches!(
            &conversation.feed()[0],
            FeedItem::OutgoingUser { text, state: OutgoingState::Sent } if text == "hello"
        ));
    }

    #[test]
    fn a_failed_send_keeps_the_draft_and_drops_the_local_row() {
        let mut conversation = Conversation::for_session("ses_a");
        conversation.prompt.set_value("draft".to_string());
        conversation.queue_prompt("draft".to_string(), "ses_a");
        conversation.fail_prompt();

        assert_eq!(conversation.prompt.value(), "draft");
        assert!(conversation.outgoing.is_empty());
        assert!(conversation.activity_indicator().is_none());
    }

    #[test]
    fn a_second_prompt_appends_a_second_user_row_in_order() {
        let mut conversation = Conversation::for_session("ses_a");
        conversation.queue_prompt("first".to_string(), "ses_a");
        conversation.accept_prompt();
        conversation.push_event(push(
            1,
            "ses_a",
            AgentStateEvent::TextDelta {
                role: MessageRole::Assistant,
                text: "one".to_string(),
            },
        ));
        conversation.queue_prompt("second".to_string(), "ses_a");
        conversation.accept_prompt();

        let outgoing: Vec<_> = conversation
            .feed()
            .into_iter()
            .filter_map(|item| match item {
                FeedItem::OutgoingUser { text, .. } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(outgoing, vec!["first".to_string(), "second".to_string()]);
    }

    #[test]
    fn an_agent_echo_suppresses_the_local_user_row() {
        let mut conversation = Conversation::for_session("ses_a");
        conversation.queue_prompt("hello world".to_string(), "ses_a");
        conversation.accept_prompt();
        conversation.push_event(push(
            1,
            "ses_a",
            AgentStateEvent::TextDelta {
                role: MessageRole::User,
                text: "hello world".to_string(),
            },
        ));

        let feed = conversation.feed();
        assert!(
            !feed
                .iter()
                .any(|item| matches!(item, FeedItem::OutgoingUser { .. })),
            "the agent echo supersedes the local row"
        );
        assert_eq!(
            feed.iter()
                .filter(|item| matches!(
                    item,
                    FeedItem::Message {
                        role: MessageRole::User,
                        ..
                    }
                ))
                .count(),
            1,
            "the prompt is never shown twice"
        );
    }

    #[test]
    fn every_activity_variant_maps_to_a_label_or_none() {
        let mut conversation = Conversation::for_session("ses_a");
        let cases = [
            (AgentActivity::Starting, Some("Starting…")),
            (AgentActivity::Thinking, Some("Thinking")),
            (AgentActivity::Responding, Some("Responding")),
            (
                AgentActivity::Tool {
                    name: "read".to_string(),
                    description: None,
                },
                Some("Running read"),
            ),
            (
                AgentActivity::Waiting {
                    request: InputRequest {
                        id: "perm_1".to_string(),
                        kind: AwaitingInputKind::Permission,
                        message: "Allow?".to_string(),
                        options: vec!["yes".to_string()],
                        allow_always: false,
                        form: None,
                    },
                },
                Some("Waiting for you"),
            ),
            (AgentActivity::Idle, None),
            (AgentActivity::Exited { code: Some(0) }, None),
        ];
        for (activity, expected) in cases {
            conversation.apply_activity(Some(activity));
            assert_eq!(
                conversation.activity_indicator().map(|a| a.label),
                expected.map(str::to_string)
            );
        }
    }

    #[test]
    fn idle_and_terminal_status_clear_the_activity() {
        let mut conversation = Conversation::for_session("ses_a");
        conversation.queue_prompt("hello".to_string(), "ses_a");
        conversation.accept_prompt();
        assert!(conversation.activity_indicator().is_some());

        conversation.apply_activity(Some(AgentActivity::Idle));
        assert!(conversation.activity_indicator().is_none());

        conversation.apply_activity(Some(AgentActivity::Thinking));
        conversation.mark_status(true, false);
        assert!(conversation.activity_indicator().is_none());
    }

    #[test]
    fn structured_events_keep_the_activity_honest_without_a_state_push() {
        let mut conversation = Conversation::for_session("ses_a");
        conversation.push_event(push(1, "ses_a", AgentStateEvent::TurnStarted));
        assert_eq!(
            conversation.activity_indicator().map(|a| a.label),
            Some("Thinking".to_string())
        );
        conversation.push_event(push(
            2,
            "ses_a",
            AgentStateEvent::ToolStarted {
                id: "call_1".to_string(),
                name: "read".to_string(),
                input: serde_json::Value::Null,
                detail: None,
            },
        ));
        assert_eq!(
            conversation.activity_indicator().map(|a| a.label),
            Some("Running read".to_string())
        );
        conversation.push_event(push(
            3,
            "ses_a",
            AgentStateEvent::Idle {
                outcome: favetto_wire::model::IdleOutcome::Succeeded,
            },
        ));
        assert!(conversation.activity_indicator().is_none());
    }

    #[test]
    fn a_stale_idle_snapshot_does_not_cancel_a_just_accepted_prompt() {
        let mut conversation = Conversation::for_session("ses_a");
        conversation.queue_prompt("hello".to_string(), "ses_a");
        conversation.accept_prompt();

        let mut live = session("ses_a", None);
        live.activity = Some(AgentActivity::Idle);
        conversation.sync_activity(&[live.clone()]);
        assert_eq!(
            conversation.activity_indicator().map(|a| a.label),
            Some("Thinking".to_string()),
            "a lagging roster must not blank the new turn's spinner"
        );

        live.activity = Some(AgentActivity::Responding);
        conversation.sync_activity(&[live]);
        assert_eq!(
            conversation.activity_indicator().map(|a| a.label),
            Some("Responding".to_string())
        );
    }

    #[test]
    fn a_local_prompt_stays_in_the_durable_summary() {
        let mut conversation = Conversation::for_session("ses_a");
        conversation.queue_prompt("remember this".to_string(), "ses_a");
        conversation.accept_prompt();
        let items = conversation.summary_feed(&RunSummary {
            text: "assistant answer".to_string(),
            ..Default::default()
        });
        assert!(matches!(
            &items[0],
            FeedItem::OutgoingUser { text, .. } if text == "remember this"
        ));
        assert!(matches!(
            &items[1],
            FeedItem::Message {
                role: MessageRole::Assistant,
                ..
            }
        ));
    }
}
