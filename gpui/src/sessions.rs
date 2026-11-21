//! The persisted ACP session history shown by the Settings Agents card.
//!
//! The daemon remains the only session store: this module holds the latest
//! `agents.history` snapshot plus the client-side state of an `agents.load`
//! request. Every load is addressed by the Favetto-local session id; the
//! provider's ACP id is never part of the client model.
//!
//! The state machine is GPUI-free and unit-tested without a window.

use std::collections::HashMap;

use base64::Engine;
use favetto_wire::model::{
    AgentSessionHistoryEntry, AgentSessionInfo, AgentSessionState, ProviderSessionEntry,
    ProviderSessionsResult,
};
use favetto_wire::rpc::AgentAttachResult;

/// How much of a restored transcript the Settings card renders. The agent owns
/// the full conversation; this is a bounded preview, never a stored copy.
pub const TRANSCRIPT_PREVIEW_CHARS: usize = 4_000;

/// The effective state of one history row: the persisted lifecycle plus any
/// load request made from this client.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionRowState {
    /// A live ACP process exists; the row is informational.
    Active,
    /// A restore is in progress (`session/load` replaying).
    Loading,
    /// The session ended and can be loaded.
    Ended,
    /// Provider-owned history that has not been imported yet; it is resumable
    /// when the agent advertises load/resume.
    Provider,
    /// This client loaded the session successfully in the current snapshot.
    Ready,
    /// A load failed, with the daemon's or transport's message.
    Failed(String),
    /// The row cannot be loaded; the reason comes from `agents.history`.
    Unsupported(String),
}

impl SessionRowState {
    /// A short, non-color-only label for the row.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Loading => "loading",
            Self::Ended => "ended",
            Self::Provider => "provider",
            Self::Ready => "ready",
            Self::Failed(_) => "failed",
            Self::Unsupported(_) => "cannot load",
        }
    }

    /// The failure/unsupported detail shown under the state, if any.
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Failed(message) | Self::Unsupported(message) => Some(message),
            _ => None,
        }
    }
}

/// One load request made from this client, keyed by Favetto-local id.
#[derive(Debug, Clone, PartialEq)]
enum LoadRequest {
    Loading,
    Ready {
        session: Box<AgentSessionInfo>,
        /// The replayed plain-text transcript that came back with the load.
        transcript: String,
    },
    Failed(String),
}

/// The most recent `agents.history` snapshot plus in-flight load requests.
#[derive(Debug, Default, Clone)]
pub struct SessionHistory {
    entries: Vec<AgentSessionHistoryEntry>,
    loads: HashMap<String, LoadRequest>,
    /// Per-agent provider-session pages from `agents.provider_sessions`.
    provider: Vec<ProviderSessionsResult>,
    /// Provider resume/delete state keyed by `(agent, provider session id)`.
    provider_actions: HashMap<(String, String), ProviderAction>,
}

/// One provider-row action made from this client.
#[derive(Debug, Clone, PartialEq)]
enum ProviderAction {
    Resuming,
    Ready {
        session: Box<AgentSessionInfo>,
        transcript: String,
    },
    Deleting,
    Deleted,
    Failed(String),
}

fn provider_key(agent: &str, session_id: &str) -> (String, String) {
    (agent.to_string(), session_id.to_string())
}

impl SessionHistory {
    /// Replace the snapshot, retaining load results for sessions that are still
    /// listed. A vanished row's state is dropped rather than shown against a
    /// different record with a reused id.
    pub fn set_entries(&mut self, entries: Vec<AgentSessionHistoryEntry>) {
        let ids: std::collections::HashSet<&str> =
            entries.iter().map(|entry| entry.id.as_str()).collect();
        self.loads.retain(|id, _| ids.contains(id.as_str()));
        self.entries = entries;
    }

    /// The current history rows, newest-updated first as the daemon returns them.
    pub fn entries(&self) -> &[AgentSessionHistoryEntry] {
        &self.entries
    }

    /// Replace the provider-session snapshot, retaining action state for rows
    /// that are still listed. Provider rows are never merged into `entries`.
    pub fn set_provider_snapshot(&mut self, provider: Vec<ProviderSessionsResult>) {
        let live: std::collections::HashSet<(String, String)> = provider
            .iter()
            .flat_map(|result| {
                result
                    .sessions
                    .iter()
                    .map(|entry| provider_key(&entry.agent, &entry.session_id))
            })
            .collect();
        self.provider_actions.retain(|key, _| live.contains(key));
        self.provider = provider;
    }

    /// Per-agent provider history and capability notices.
    pub fn provider_results(&self) -> &[ProviderSessionsResult] {
        &self.provider
    }

    /// Find a provider row by its agent and ACP id.
    pub fn provider_entry(&self, agent: &str, session_id: &str) -> Option<&ProviderSessionEntry> {
        self.provider
            .iter()
            .find(|result| result.agent == agent)
            .and_then(|result| {
                result
                    .sessions
                    .iter()
                    .find(|entry| entry.session_id == session_id)
            })
    }

    /// The capability flags for one agent's provider history.
    pub fn provider_capabilities(&self, agent: &str) -> (bool, bool) {
        self.provider
            .iter()
            .find(|result| result.agent == agent)
            .map(|result| (result.can_resume, result.can_delete))
            .unwrap_or((false, false))
    }

    /// The effective display state of one provider-owned row.
    pub fn provider_state(&self, entry: &ProviderSessionEntry) -> SessionRowState {
        let key = provider_key(&entry.agent, &entry.session_id);
        match self.provider_actions.get(&key) {
            Some(ProviderAction::Resuming) | Some(ProviderAction::Deleting) => {
                return SessionRowState::Loading;
            }
            Some(ProviderAction::Ready { .. }) => return SessionRowState::Ready,
            Some(ProviderAction::Deleted) => {
                return SessionRowState::Unsupported(
                    "the provider session was deleted".to_string(),
                );
            }
            Some(ProviderAction::Failed(message)) => {
                return SessionRowState::Failed(message.clone());
            }
            None => {}
        }
        let (can_resume, _) = self.provider_capabilities(&entry.agent);
        if can_resume {
            SessionRowState::Provider
        } else {
            SessionRowState::Unsupported(format!(
                "agent '{}' does not advertise ACP session/load or session/resume",
                entry.agent
            ))
        }
    }

    /// Whether this row can be resumed right now.
    pub fn can_resume_provider(&self, entry: &ProviderSessionEntry) -> bool {
        let (can_resume, _) = self.provider_capabilities(&entry.agent);
        if !can_resume {
            return false;
        }
        !matches!(
            self.provider_actions
                .get(&provider_key(&entry.agent, &entry.session_id)),
            Some(ProviderAction::Resuming)
                | Some(ProviderAction::Ready { .. })
                | Some(ProviderAction::Deleting)
        )
    }

    /// Whether this row can be deleted right now.
    pub fn can_delete_provider(&self, entry: &ProviderSessionEntry) -> bool {
        let (_, can_delete) = self.provider_capabilities(&entry.agent);
        can_delete
            && !matches!(
                self.provider_actions
                    .get(&provider_key(&entry.agent, &entry.session_id)),
                Some(ProviderAction::Resuming)
                    | Some(ProviderAction::Deleting)
                    | Some(ProviderAction::Deleted)
            )
    }

    /// Start a provider resume; returns `false` when the row is not resumable.
    pub fn begin_provider_resume(&mut self, agent: &str, session_id: &str) -> bool {
        let Some(entry) = self.provider_entry(agent, session_id) else {
            return false;
        };
        if !self.can_resume_provider(entry) {
            return false;
        }
        self.provider_actions
            .insert(provider_key(agent, session_id), ProviderAction::Resuming);
        true
    }

    /// Record a completed `agents.provider_resume`.
    pub fn finish_provider_resume(
        &mut self,
        agent: &str,
        session_id: &str,
        result: Result<AgentAttachResult, String>,
    ) {
        let action = match result {
            Ok(attached) => {
                match base64::engine::general_purpose::STANDARD.decode(&attached.data) {
                    Ok(bytes) => match String::from_utf8(bytes) {
                        Ok(transcript) => ProviderAction::Ready {
                            session: Box::new(attached.session),
                            transcript,
                        },
                        Err(_) => ProviderAction::Failed(
                            "the daemon returned a transcript that is not valid UTF-8".to_string(),
                        ),
                    },
                    Err(_) => ProviderAction::Failed(
                        "the daemon returned an unreadable session transcript".to_string(),
                    ),
                }
            }
            Err(message) => ProviderAction::Failed(message),
        };
        self.provider_actions
            .insert(provider_key(agent, session_id), action);
    }

    /// Start a provider delete; returns `false` when unsupported or in flight.
    pub fn begin_provider_delete(&mut self, agent: &str, session_id: &str) -> bool {
        let Some(entry) = self.provider_entry(agent, session_id) else {
            return false;
        };
        if !self.can_delete_provider(entry) {
            return false;
        }
        self.provider_actions
            .insert(provider_key(agent, session_id), ProviderAction::Deleting);
        true
    }

    /// Record a completed `agents.provider_delete`.
    pub fn finish_provider_delete(
        &mut self,
        agent: &str,
        session_id: &str,
        result: Result<(), String>,
    ) {
        let action = match result {
            Ok(()) => ProviderAction::Deleted,
            Err(message) => ProviderAction::Failed(message),
        };
        self.provider_actions
            .insert(provider_key(agent, session_id), action);
    }

    /// The provider-resume replay for a row, when it succeeded.
    pub fn provider_transcript(&self, agent: &str, session_id: &str) -> Option<&str> {
        match self.provider_actions.get(&provider_key(agent, session_id)) {
            Some(ProviderAction::Ready { transcript, .. }) => Some(transcript),
            _ => None,
        }
    }

    /// A bounded, display-only provider transcript preview.
    pub fn provider_transcript_preview(&self, agent: &str, session_id: &str) -> Option<String> {
        self.provider_transcript(agent, session_id)
            .map(preview_transcript)
    }

    /// The effective state of one row.
    pub fn state(&self, entry: &AgentSessionHistoryEntry) -> SessionRowState {
        match self.loads.get(&entry.id) {
            Some(LoadRequest::Loading) => return SessionRowState::Loading,
            Some(LoadRequest::Ready { session, .. }) => {
                // A ready restore is only meaningful for the same local record.
                if session.id == entry.id {
                    return SessionRowState::Ready;
                }
            }
            Some(LoadRequest::Failed(message)) => {
                return SessionRowState::Failed(message.clone());
            }
            None => {}
        }

        match entry.state {
            AgentSessionState::Active => SessionRowState::Active,
            AgentSessionState::Loading => SessionRowState::Loading,
            AgentSessionState::Failed => SessionRowState::Failed(
                entry
                    .load_error
                    .clone()
                    .unwrap_or_else(|| "the session failed before it could be loaded".to_string()),
            ),
            AgentSessionState::Ended if entry.loadable => SessionRowState::Ended,
            AgentSessionState::Ended => SessionRowState::Unsupported(
                entry
                    .load_error
                    .clone()
                    .unwrap_or_else(|| "this session cannot be restored".to_string()),
            ),
        }
    }

    /// Whether selecting this row should issue `agents.load`.
    ///
    /// A failed client restore stays retryable (the record itself is still
    /// loadable); a failed or unsupported record never is.
    pub fn can_load(&self, entry: &AgentSessionHistoryEntry) -> bool {
        if entry.state != AgentSessionState::Ended || !entry.loadable {
            return false;
        }
        !matches!(self.loads.get(&entry.id), Some(LoadRequest::Loading))
            && !matches!(self.loads.get(&entry.id), Some(LoadRequest::Ready { .. }))
    }

    /// Start a load for `id`, or return `false` when it is not loadable.
    pub fn begin_load(&mut self, id: &str) -> bool {
        let Some(entry) = self.entries.iter().find(|entry| entry.id == id) else {
            return false;
        };
        if !self.can_load(entry) {
            return false;
        }
        self.loads.insert(id.to_string(), LoadRequest::Loading);
        true
    }

    /// Record a completed `agents.load`. Success stores the attached session and
    /// its replayed transcript so the row can show `ready`; failure keeps the
    /// daemon's message and leaves the row retryable.
    pub fn finish_load(&mut self, id: &str, result: Result<AgentAttachResult, String>) {
        match result {
            Ok(attached) if attached.session.id == id => {
                let request = match base64::engine::general_purpose::STANDARD.decode(&attached.data)
                {
                    Ok(bytes) => match String::from_utf8(bytes) {
                        Ok(transcript) => LoadRequest::Ready {
                            session: Box::new(attached.session),
                            transcript,
                        },
                        Err(_) => LoadRequest::Failed(
                            "the daemon returned a transcript that is not valid UTF-8".to_string(),
                        ),
                    },
                    Err(_) => LoadRequest::Failed(
                        "the daemon returned an unreadable session transcript".to_string(),
                    ),
                };
                self.loads.insert(id.to_string(), request);
            }
            Ok(attached) => {
                self.loads.insert(
                    id.to_string(),
                    LoadRequest::Failed(format!(
                        "the daemon loaded session '{}' instead of '{id}'",
                        attached.session.id
                    )),
                );
            }
            Err(message) => {
                self.loads
                    .insert(id.to_string(), LoadRequest::Failed(message));
            }
        }
    }

    /// The restored plain-text transcript for a successfully loaded row.
    ///
    /// `None` until the daemon confirms the load, so a partial replay or a
    /// failed restore can never be rendered as restored history.
    pub fn transcript(&self, id: &str) -> Option<&str> {
        match self.loads.get(id) {
            Some(LoadRequest::Ready { transcript, .. }) => Some(transcript),
            _ => None,
        }
    }

    /// A bounded, display-only preview of the restored transcript.
    pub fn transcript_preview(&self, id: &str) -> Option<String> {
        self.transcript(id).map(preview_transcript)
    }

    /// The loaded session for a row, when this client restored it successfully.
    #[cfg(test)]
    pub fn loaded(&self, id: &str) -> Option<&AgentSessionInfo> {
        match self.loads.get(id) {
            Some(LoadRequest::Ready { session, .. }) => Some(session.as_ref()),
            _ => None,
        }
    }

    /// Whether a load for `id` is currently in flight.
    #[cfg(test)]
    pub fn is_loading(&self, id: &str) -> bool {
        matches!(self.loads.get(id), Some(LoadRequest::Loading))
    }
}

/// Bound the displayed transcript so a long restored conversation cannot make
/// the Settings card expensive to paint. The full text stays with the daemon.
fn preview_transcript(transcript: &str) -> String {
    if transcript.chars().count() <= TRANSCRIPT_PREVIEW_CHARS {
        return transcript.to_string();
    }
    let head: String = transcript.chars().take(TRANSCRIPT_PREVIEW_CHARS).collect();
    format!("{head}\n… (transcript truncated for display)")
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use favetto_wire::model::{AgentBackend, AgentSessionState};

    use super::*;

    fn entry(id: &str, state: AgentSessionState, loadable: bool) -> AgentSessionHistoryEntry {
        AgentSessionHistoryEntry {
            id: id.to_string(),
            agent: "opencode".to_string(),
            title: Some(format!("session {id}")),
            cwd: std::path::PathBuf::from("/work"),
            task_id: None,
            state,
            loadable,
            load_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn session(id: &str) -> AgentSessionInfo {
        AgentSessionInfo {
            id: id.to_string(),
            agent: "opencode".to_string(),
            backend: AgentBackend::Acp,
            task_id: None,
            running: true,
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
        }
    }

    fn attached(id: &str, text: &str) -> AgentAttachResult {
        AgentAttachResult {
            session: session(id),
            data: base64::engine::general_purpose::STANDARD.encode(text.as_bytes()),
            seq: 3,
            plan: Vec::new(),
            commands: Vec::new(),
        }
    }

    #[test]
    fn history_distinguishes_active_ended_failed_and_unsupported() {
        let mut history = SessionHistory::default();
        history.set_entries(vec![
            entry("active", AgentSessionState::Active, false),
            entry("ended", AgentSessionState::Ended, true),
            entry("failed", AgentSessionState::Failed, false),
            entry("unsupported", AgentSessionState::Ended, false),
            entry("loading", AgentSessionState::Loading, false),
        ]);

        assert_eq!(
            history.state(&history.entries()[0]),
            SessionRowState::Active
        );
        assert_eq!(history.state(&history.entries()[1]), SessionRowState::Ended);
        assert!(matches!(
            history.state(&history.entries()[2]),
            SessionRowState::Failed(_)
        ));
        assert!(matches!(
            history.state(&history.entries()[3]),
            SessionRowState::Unsupported(_)
        ));
        assert_eq!(
            history.state(&history.entries()[4]),
            SessionRowState::Loading
        );
        assert!(history.can_load(&history.entries()[1]));
        assert!(!history.can_load(&history.entries()[0]));
        assert!(!history.can_load(&history.entries()[2]));
        assert!(!history.can_load(&history.entries()[3]));
    }

    #[test]
    fn load_is_addressed_by_the_local_id_and_transitions_to_ready() {
        let mut history = SessionHistory::default();
        history.set_entries(vec![
            entry("local-past", AgentSessionState::Ended, true),
            entry("local-failed", AgentSessionState::Failed, false),
        ]);

        assert!(!history.begin_load("local-failed"));
        assert!(!history.begin_load("provider-id"));
        assert!(history.begin_load("local-past"));
        assert!(history.is_loading("local-past"));
        assert_eq!(
            history.state(&history.entries()[0]),
            SessionRowState::Loading
        );

        history.finish_load(
            "local-past",
            Ok(attached("local-past", "restored question\nhello")),
        );
        assert_eq!(history.state(&history.entries()[0]), SessionRowState::Ready);
        assert_eq!(
            history.loaded("local-past").map(|s| s.id.as_str()),
            Some("local-past")
        );
        assert_eq!(
            history.transcript("local-past"),
            Some("restored question\nhello"),
            "the replay is shown only after the load succeeds"
        );
        assert!(!history.can_load(&history.entries()[0]));
    }

    #[test]
    fn a_replay_is_absent_while_loading_and_after_a_failure() {
        let mut history = SessionHistory::default();
        history.set_entries(vec![entry("local-past", AgentSessionState::Ended, true)]);

        assert!(history.begin_load("local-past"));
        assert!(history.is_loading("local-past"));
        assert_eq!(
            history.transcript("local-past"),
            None,
            "an in-flight load exposes no replay"
        );

        history.finish_load("local-past", Err("agent could not load".to_string()));
        assert_eq!(
            history.transcript("local-past"),
            None,
            "a failed load discards its replay"
        );
        assert!(matches!(
            history.state(&history.entries()[0]),
            SessionRowState::Failed(_)
        ));

        assert!(history.begin_load("local-past"));
        history.finish_load("local-past", Ok(attached("local-past", "complete replay")));
        assert_eq!(history.transcript("local-past"), Some("complete replay"));
    }

    #[test]
    fn a_load_for_another_local_id_fails_without_exposing_its_replay() {
        let mut history = SessionHistory::default();
        history.set_entries(vec![entry("local-past", AgentSessionState::Ended, true)]);
        assert!(history.begin_load("local-past"));

        history.finish_load(
            "local-past",
            Ok(attached("provider-id", "wrong transcript")),
        );
        assert_eq!(history.transcript("local-past"), None);
        let state = history.state(&history.entries()[0]);
        assert!(
            matches!(&state, SessionRowState::Failed(message) if message.contains("provider-id")),
            "{state:?}"
        );
        assert!(history.can_load(&history.entries()[0]), "still retryable");
    }

    #[test]
    fn an_unreadable_replay_is_reported_instead_of_rendered() {
        let mut history = SessionHistory::default();
        history.set_entries(vec![entry("local-past", AgentSessionState::Ended, true)]);
        assert!(history.begin_load("local-past"));

        history.finish_load(
            "local-past",
            Ok(AgentAttachResult {
                session: session("local-past"),
                data: "not base64!".to_string(),
                seq: 0,
                plan: Vec::new(),
                commands: Vec::new(),
            }),
        );
        assert_eq!(history.transcript("local-past"), None);
        let state = history.state(&history.entries()[0]);
        assert!(
            matches!(&state, SessionRowState::Failed(message) if message.contains("unreadable")),
            "{state:?}"
        );
    }

    #[test]
    fn the_transcript_preview_is_bounded() {
        let long = "x".repeat(TRANSCRIPT_PREVIEW_CHARS + 10);
        let preview = preview_transcript(&long);
        assert!(preview.starts_with(&"x".repeat(TRANSCRIPT_PREVIEW_CHARS)));
        assert!(preview.contains("truncated for display"));
        assert!(preview.chars().count() > TRANSCRIPT_PREVIEW_CHARS);
        assert_eq!(preview_transcript("short"), "short");
    }

    #[test]
    fn a_failed_load_keeps_the_reason_and_stays_retryable() {
        let mut history = SessionHistory::default();
        history.set_entries(vec![entry("local-past", AgentSessionState::Ended, true)]);
        assert!(history.begin_load("local-past"));
        history.finish_load(
            "local-past",
            Err(
                "agent 'opencode' does not support loading ACP sessions (session/load)".to_string(),
            ),
        );

        let state = history.state(&history.entries()[0]);
        assert!(matches!(state, SessionRowState::Failed(_)));
        assert!(
            state
                .detail()
                .is_some_and(|message| message.contains("does not support")),
            "{state:?}"
        );
        // A transport/restore failure does not corrupt the record: the same
        // ended, loadable row can be retried.
        assert!(history.can_load(&history.entries()[0]));
        assert!(history.begin_load("local-past"));
    }

    fn provider(id: &str) -> favetto_wire::model::ProviderSessionEntry {
        favetto_wire::model::ProviderSessionEntry {
            agent: "opencode".to_string(),
            session_id: id.to_string(),
            cwd: std::path::PathBuf::from("/work"),
            title: Some(format!("provider {id}")),
            updated_at: None,
            local_session_id: None,
        }
    }

    fn provider_result(can_resume: bool, can_delete: bool) -> ProviderSessionsResult {
        ProviderSessionsResult {
            agent: "opencode".to_string(),
            supported: true,
            can_resume,
            can_delete,
            sessions: vec![provider("provider-1")],
            next_cursor: None,
            error: None,
        }
    }

    #[test]
    fn provider_rows_are_gated_and_import_only_on_resume() {
        let mut history = SessionHistory::default();
        history.set_provider_snapshot(vec![provider_result(true, true)]);
        let entry = history
            .provider_entry("opencode", "provider-1")
            .unwrap()
            .clone();

        assert!(history.can_resume_provider(&entry));
        assert!(history.can_delete_provider(&entry));
        assert_eq!(history.provider_state(&entry), SessionRowState::Provider);

        assert!(history.begin_provider_resume("opencode", "provider-1"));
        assert_eq!(history.provider_state(&entry), SessionRowState::Loading);
        assert!(!history.begin_provider_delete("opencode", "provider-1"));

        // A session/resume path has no replay: readiness is still real, but the
        // provider text stays empty rather than pretending the load replayed.
        history.finish_provider_resume(
            "opencode",
            "provider-1",
            Ok(AgentAttachResult {
                session: session("local-imported"),
                data: String::new(),
                seq: 0,
                plan: Vec::new(),
                commands: Vec::new(),
            }),
        );
        assert_eq!(history.provider_state(&entry), SessionRowState::Ready);
        assert_eq!(
            history.provider_transcript("opencode", "provider-1"),
            Some("")
        );
        assert!(!history.can_resume_provider(&entry));
    }

    #[test]
    fn provider_capabilities_hide_unsupported_actions() {
        let mut history = SessionHistory::default();
        history.set_provider_snapshot(vec![provider_result(false, false)]);
        let entry = history
            .provider_entry("opencode", "provider-1")
            .unwrap()
            .clone();

        assert!(!history.can_resume_provider(&entry));
        assert!(!history.can_delete_provider(&entry));
        assert!(matches!(
            history.provider_state(&entry),
            SessionRowState::Unsupported(_)
        ));
        assert!(!history.begin_provider_resume("opencode", "provider-1"));
        assert!(!history.begin_provider_delete("opencode", "provider-1"));
    }

    #[test]
    fn a_provider_delete_is_distinct_from_the_local_record() {
        let mut history = SessionHistory::default();
        history.set_provider_snapshot(vec![provider_result(true, true)]);
        history.set_entries(vec![entry("local-past", AgentSessionState::Ended, true)]);
        let entry = history
            .provider_entry("opencode", "provider-1")
            .unwrap()
            .clone();

        assert!(history.begin_provider_delete("opencode", "provider-1"));
        history.finish_provider_delete("opencode", "provider-1", Ok(()));
        assert!(matches!(
            history.provider_state(&entry),
            SessionRowState::Unsupported(_)
        ));
        // Favetto's own durable row is untouched.
        assert_eq!(history.entries().len(), 1);
        assert!(history.can_load(&history.entries()[0]));
    }

    #[test]
    fn replacing_the_snapshot_drops_vanished_load_results() {
        let mut history = SessionHistory::default();
        history.set_entries(vec![entry("local-past", AgentSessionState::Ended, true)]);
        assert!(history.begin_load("local-past"));
        history.finish_load("local-past", Ok(attached("local-past", "replay")));

        history.set_entries(vec![entry("other", AgentSessionState::Ended, true)]);
        assert!(history.loaded("local-past").is_none());
        assert!(history.begin_load("other"));
    }
}
