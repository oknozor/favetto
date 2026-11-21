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
use favetto_wire::model::{AgentSessionHistoryEntry, AgentSessionInfo, AgentSessionState};
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
