//! The pure task-detail model: persisted output, the structured event feed and
//! the per-attempt run table.
//!
//! The detail surface is the client's core surface (docs/design/gpui-client.md
//! §5, step 06), so everything that can be decided without a window lives here:
//! output extraction, feed grouping, usage formatting, the permission banner's
//! request resolution and the per-tab load state. [`crate::views::detail`] only
//! paints it.
//!
//! Two forward-compatibility rules matter:
//!
//! - Unknown *push methods* and unknown *event kinds* are skipped, never fatal
//!   ([`crate::sse`] and [`DetailEventsResult`]);
//! - An absent or unrenderable output blob becomes the Output tab's empty state,
//!   not an error.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use favetto_wire::elicitation::ElicitationRequest;
use favetto_wire::model::{
    AgentCommand, AgentPlanEntry, AgentSessionInfo, AgentStateEvent, AgentUsage, AwaitingInputKind,
    ContentRole, IdleOutcome, InputRequest, MessageContent, MessageRole, RunStatus, Task, TaskRun,
    TaskStatus,
};
use favetto_wire::rpc::{AgentEventPush, RpcCall, TasksEventsParams};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::inbox::{self, StatusKind};
use crate::tasks;

/// The maximum number of event frames retained for one open detail. The daemon
/// already caps the backfill at 2000; this keeps a long-running live feed from
/// growing without bound while the window stays open.
pub const MAX_RETAINED_EVENTS: usize = 2_000;

/// The three tabs of the detail surface (docs §2, Miller's Law: three chunks).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DetailTab {
    /// The persisted `tasks.get` output.
    #[default]
    Output,
    /// The structured `tasks.events` feed plus live `agent.event` pushes.
    Events,
    /// The `tasks.runs` attempt table.
    Runs,
}

impl DetailTab {
    /// Every tab, in bar order.
    pub const ALL: [DetailTab; 3] = [DetailTab::Output, DetailTab::Events, DetailTab::Runs];

    /// The tab label.
    pub fn label(self) -> &'static str {
        match self {
            DetailTab::Output => "Output",
            DetailTab::Events => "Events",
            DetailTab::Runs => "Runs",
        }
    }
}

/// The persisted output blob, already reduced to plain text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputText {
    /// The text to render, with terminal escapes removed.
    pub text: String,
    /// Whether the daemon flagged the blob as truncated.
    pub truncated: bool,
}

/// Extract the human-readable text from a `tasks.get` output value.
///
/// The executor stores an envelope (`{ "output": "…", "truncated": bool }`); a
/// bare string and any other JSON shape are rendered as text too. A `null` or
/// empty output has nothing to show and returns `None`, so the Output tab can
/// render its "no output yet" state (which is not an error).
pub fn extract_output(value: Option<&serde_json::Value>) -> Option<OutputText> {
    let value = value?;
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::String(text) => {
            let text = strip_ansi(text);
            (!text.trim().is_empty()).then_some(OutputText {
                text,
                truncated: false,
            })
        }
        serde_json::Value::Object(map) => match map.get("output") {
            Some(serde_json::Value::String(text)) => {
                let truncated = map
                    .get("truncated")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                let text = strip_ansi(text);
                (!text.trim().is_empty() || truncated).then_some(OutputText { text, truncated })
            }
            _ => {
                let text = pretty_json(value);
                (!text.trim().is_empty()).then_some(OutputText {
                    text,
                    truncated: false,
                })
            }
        },
        _ => {
            let text = pretty_json(value);
            (!text.trim().is_empty()).then_some(OutputText {
                text,
                truncated: false,
            })
        }
    }
}

/// Pretty-print a JSON value for the plain-text view.
pub fn pretty_json(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// Remove ANSI/VT100 escape sequences so the output pane shows plain text.
///
/// This is deliberately not a terminal emulator: it only deletes colour and
/// cursor control sequences. Structured events are the supported live surface
/// (docs §6, scope), and this keeps a persisted log readable.
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\u{1b}' {
            out.push(character);
            continue;
        }
        match chars.peek().copied() {
            // CSI: ESC [ parameters final
            Some('[') => {
                chars.next();
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            // OSC: ESC ] … BEL or ST (ESC \)
            Some(']') => {
                chars.next();
                while let Some(next) = chars.next() {
                    if next == '\u{07}' {
                        break;
                    }
                    if next == '\u{1b}' {
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                }
            }
            // A lone two-byte escape.
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    out
}

/// A token count in the compact form used by the header and run table.
pub fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{}M tok", trimmed(tokens as f64 / 1_000_000.0))
    } else if tokens >= 1_000 {
        format!("{}k tok", trimmed(tokens as f64 / 1_000.0))
    } else {
        format!("{tokens} tok")
    }
}

fn trimmed(value: f64) -> String {
    let rendered = if value >= 10.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    };
    rendered.replace(".0", "")
}

/// A cost/token summary for the header and the Runs table, when known.
pub fn usage_summary(usage: Option<&AgentUsage>) -> Option<String> {
    let usage = usage?;
    let mut parts = Vec::new();
    if let Some(cost) = usage
        .reported_cost()
        .as_ref()
        .and_then(|cost| inbox::format_cost(Some(cost)))
    {
        parts.push(cost);
    }
    let tokens = usage.total_tokens();
    if tokens > 0 {
        parts.push(format_tokens(tokens));
    }
    if parts.is_empty() {
        if let Some(used) = usage.context_used {
            let size = usage
                .context_size
                .map(|size| format!("/{}", format_tokens(size).replace(" tok", "")))
                .unwrap_or_default();
            parts.push(format!(
                "ctx {}{size}",
                format_tokens(used).replace(" tok", "")
            ));
        }
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// How a tool call renders in the feed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolStatus {
    /// Started but not finished.
    Running,
    /// Finished successfully.
    Ok,
    /// Finished with an error.
    Error,
}

impl ToolStatus {
    /// The chip label.
    pub fn label(self) -> &'static str {
        match self {
            ToolStatus::Running => "running",
            ToolStatus::Ok => "ok",
            ToolStatus::Error => "error",
        }
    }

    /// The chip glyph.
    pub fn icon(self) -> &'static str {
        match self {
            ToolStatus::Running => "◐",
            ToolStatus::Ok => "✓",
            ToolStatus::Error => "✕",
        }
    }
}

/// One renderable entry in the structured event feed.
#[derive(Clone, Debug, PartialEq)]
pub enum FeedItem {
    /// A (grouped) message block.
    Message {
        /// Who produced the text.
        role: MessageRole,
        /// The accumulated deltas.
        text: String,
    },
    /// Chain-of-thought text, rendered muted.
    Reasoning {
        /// The accumulated deltas.
        text: String,
        /// True while this is the trailing reasoning run.
        streaming: bool,
    },
    /// A non-text message block, rendered as a safe placeholder.
    ContentBlock {
        /// Which message stream carried the block.
        role: ContentRole,
        /// Metadata-only block (no binary payload).
        content: MessageContent,
    },
    /// A tool invocation, from start to finish.
    Tool {
        /// The agent's correlation id.
        id: String,
        /// The tool name.
        name: String,
        /// The invocation input.
        input: serde_json::Value,
        /// The current status.
        status: ToolStatus,
        /// The (possibly partial) output.
        output: Option<serde_json::Value>,
    },
    /// Token/cost usage for a step.
    Usage(AgentUsage),
    /// A session-level error.
    Error(String),
    /// A turn/run boundary.
    Boundary(IdleOutcome),
    /// A prompt the agent asked; `resolved` once a matching resolve/idle arrived.
    InputRequest {
        request: InputRequest,
        resolved: bool,
    },
    /// An ACP form request; `resolved` once a matching resolve/idle arrived.
    Elicitation {
        request: ElicitationRequest,
        resolved: bool,
    },
    /// The agent reported its session identity/title.
    Session { title: Option<String> },
    /// The session's title changed.
    Title(String),
}

/// Group an ordered event window into display items.
///
/// Consecutive text deltas with the same role become one [`FeedItem::Message`],
/// consecutive reasoning deltas one [`FeedItem::Reasoning`], and tool events
/// merge into the card opened by `ToolStarted`. An unknown event kind never
/// reaches this function: [`crate::sse`] and [`DetailEventsResult`] drop it
/// first, so a newer daemon cannot break the feed.
pub fn build_feed(events: &[AgentEventPush]) -> Vec<FeedItem> {
    let mut items: Vec<FeedItem> = Vec::new();
    let mut tools: HashMap<&str, usize> = HashMap::new();
    let mut pending_inputs: HashMap<&str, usize> = HashMap::new();
    let mut pending_elicitations: HashMap<&str, usize> = HashMap::new();

    for push in events {
        if !matches!(&push.event, AgentStateEvent::ReasoningDelta { .. }) {
            if let Some(FeedItem::Reasoning { streaming, .. }) = items.last_mut() {
                *streaming = false;
            }
        }

        match &push.event {
            AgentStateEvent::Session { title, .. } => items.push(FeedItem::Session {
                title: title.clone(),
            }),
            AgentStateEvent::TurnStarted => {}
            AgentStateEvent::TextDelta { role, text } => match items.last_mut() {
                Some(FeedItem::Message {
                    role: current_role,
                    text: current_text,
                }) if current_role == role => current_text.push_str(text),
                _ => items.push(FeedItem::Message {
                    role: *role,
                    text: text.clone(),
                }),
            },
            AgentStateEvent::ReasoningDelta { text } => match items.last_mut() {
                Some(FeedItem::Reasoning {
                    text: current_text, ..
                }) => current_text.push_str(text),
                _ => items.push(FeedItem::Reasoning {
                    text: text.clone(),
                    streaming: true,
                }),
            },
            AgentStateEvent::ContentBlock { role, content } => {
                items.push(FeedItem::ContentBlock {
                    role: *role,
                    content: content.clone(),
                });
            }
            AgentStateEvent::ToolStarted {
                id, name, input, ..
            } => {
                tools.insert(id.as_str(), items.len());
                items.push(FeedItem::Tool {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    status: ToolStatus::Running,
                    output: None,
                });
            }
            AgentStateEvent::ToolUpdated { id, partial, .. } => {
                if let Some(&index) = tools.get(id.as_str()) {
                    if let FeedItem::Tool { output, .. } = &mut items[index] {
                        *output = Some(partial.clone());
                    }
                } else {
                    tools.insert(id.as_str(), items.len());
                    items.push(FeedItem::Tool {
                        id: id.clone(),
                        name: String::new(),
                        input: serde_json::Value::Null,
                        status: ToolStatus::Running,
                        output: Some(partial.clone()),
                    });
                }
            }
            AgentStateEvent::ToolFinished {
                id,
                name,
                ok,
                output,
                ..
            } => {
                let status = if *ok {
                    ToolStatus::Ok
                } else {
                    ToolStatus::Error
                };
                if let Some(&index) = tools.get(id.as_str()) {
                    if let FeedItem::Tool {
                        status: slot,
                        output: slot_output,
                        name: slot_name,
                        ..
                    } = &mut items[index]
                    {
                        *slot = status;
                        if slot_name.is_empty() {
                            *slot_name = name.clone();
                        }
                        if output.is_some() {
                            *slot_output = output.clone();
                        }
                    }
                } else {
                    tools.insert(id.as_str(), items.len());
                    items.push(FeedItem::Tool {
                        id: id.clone(),
                        name: name.clone(),
                        input: serde_json::Value::Null,
                        status,
                        output: output.clone(),
                    });
                }
            }
            AgentStateEvent::InputRequested { request } => {
                pending_inputs.insert(request.id.as_str(), items.len());
                items.push(FeedItem::InputRequest {
                    request: request.clone(),
                    resolved: false,
                });
            }
            AgentStateEvent::InputResolved { id } => {
                if let Some(index) = pending_inputs.remove(id.as_str()) {
                    if let FeedItem::InputRequest { resolved, .. } = &mut items[index] {
                        *resolved = true;
                    }
                }
            }
            AgentStateEvent::ElicitationRequested { request } => {
                pending_elicitations.insert(request.id.as_str(), items.len());
                items.push(FeedItem::Elicitation {
                    request: request.clone(),
                    resolved: false,
                });
            }
            AgentStateEvent::ElicitationResolved { id, .. } => {
                if let Some(index) = pending_elicitations.remove(id.as_str()) {
                    if let FeedItem::Elicitation { resolved, .. } = &mut items[index] {
                        *resolved = true;
                    }
                }
            }
            AgentStateEvent::Usage { usage, .. } => items.push(FeedItem::Usage(usage.clone())),
            AgentStateEvent::Title { title } => items.push(FeedItem::Title(title.clone())),
            // Auth progress is rendered by the detail's pinned banner, which
            // reads the authoritative live session instead of duplicating an
            // event into the transcript.
            AgentStateEvent::Plan { .. }
            | AgentStateEvent::AvailableCommands { .. }
            | AgentStateEvent::ConfigOptions { .. }
            | AgentStateEvent::Modes { .. }
            | AgentStateEvent::CurrentMode { .. }
            | AgentStateEvent::Auth { .. } => {}
            AgentStateEvent::Idle { outcome } => {
                for index in pending_inputs.drain().map(|(_, index)| index) {
                    if let FeedItem::InputRequest { resolved, .. } = &mut items[index] {
                        *resolved = true;
                    }
                }
                for index in pending_elicitations.drain().map(|(_, index)| index) {
                    if let FeedItem::Elicitation { resolved, .. } = &mut items[index] {
                        *resolved = true;
                    }
                }
                items.push(FeedItem::Boundary(*outcome));
            }
            AgentStateEvent::Error { message } => items.push(FeedItem::Error(message.clone())),
        }
    }

    items
}

/// A compact one-line rendering of a tool input/output value.
///
/// Returns `None` for values with nothing worth showing (null, empty object or
/// array). Long JSON is cut on a character boundary with an ellipsis, because
/// the detail column is narrow and the feed is a summary, not an inspector.
pub fn compact_json(value: &serde_json::Value) -> Option<String> {
    if value.is_null() || value == &serde_json::json!({}) || value == &serde_json::json!([]) {
        return None;
    }
    let text = match value {
        serde_json::Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    };
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    const LIMIT: usize = 240;
    if text.chars().count() <= LIMIT {
        Some(text.to_string())
    } else {
        let mut cut: String = text.chars().take(LIMIT).collect();
        cut.push('…');
        Some(cut)
    }
}

/// The label for a run status (including the run-only outcomes).
pub fn run_status_label(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Pending => "pending",
        RunStatus::Running => "running",
        RunStatus::AwaitingInput => "awaiting input",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
        RunStatus::Interrupted => "interrupted",
        RunStatus::TimedOut => "timed out",
    }
}

/// The display status for a run row.
pub fn run_status_kind(status: RunStatus) -> StatusKind {
    match status {
        RunStatus::Pending => StatusKind::Pending,
        RunStatus::Running => StatusKind::Running,
        RunStatus::AwaitingInput => StatusKind::AwaitingInput,
        RunStatus::Succeeded => StatusKind::Succeeded,
        RunStatus::Failed => StatusKind::Failed,
        RunStatus::Cancelled => StatusKind::Cancelled,
        RunStatus::Interrupted | RunStatus::TimedOut => StatusKind::Neutral,
    }
}

/// A run's duration: finished minus started, or the live elapsed for a running
/// attempt. `None` until the daemon records a start time.
pub fn run_duration(run: &TaskRun, now: DateTime<Utc>) -> Option<Duration> {
    let started = run.started_at?;
    let end = run.finished_at.unwrap_or(now);
    let seconds = end.timestamp().saturating_sub(started.timestamp());
    Some(Duration::from_secs(u64::try_from(seconds).unwrap_or(0)))
}

/// The exit-code cell for a run.
pub fn run_exit_label(run: &TaskRun) -> String {
    run.exit_code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "—".to_string())
}

/// A prompt the agent is blocked on, ready for the permission banner.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingRequest {
    /// The live session that owns the request.
    pub session_id: String,
    /// The structured correlation id, when the prompt can be answered.
    pub request_id: Option<String>,
    /// What kind of decision was requested.
    pub kind: AwaitingInputKind,
    /// The prompt text.
    pub message: String,
    /// The options the prompt offers (informational).
    pub options: Vec<String>,
    /// Whether "always allow" is offered.
    pub allow_always: bool,
}

impl PendingRequest {
    /// Whether `agents.reply` can answer this prompt. Screen-detected prompts
    /// have no correlation id and are display-only (docs §6).
    pub fn can_reply(&self) -> bool {
        self.request_id.is_some() && !self.session_id.is_empty()
    }

    /// The request kind as a short label.
    pub fn kind_label(&self) -> &'static str {
        match self.kind {
            AwaitingInputKind::Permission => "permission",
            AwaitingInputKind::Confirmation => "confirmation",
            AwaitingInputKind::Choice => "choice",
            AwaitingInputKind::Pinentry => "passphrase",
            AwaitingInputKind::Elicitation => "form",
            AwaitingInputKind::Other => "input",
        }
    }
}

/// A pending ACP form elicitation, ready for the modal.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingElicitation {
    pub session_id: String,
    pub request: ElicitationRequest,
}

/// The client's per-task detail state. One lives on [`crate::state::ClientState`]
/// for the selected run and is replaced when another run opens.
#[derive(Clone, Debug)]
pub struct TaskDetail {
    /// The task this detail belongs to.
    pub task_id: Uuid,
    /// The full task from `tasks.get`, including the persisted output.
    pub task: Option<Task>,
    /// Whether the first `tasks.get` for this open is in flight.
    pub task_loading: bool,
    /// The last `tasks.get` failure.
    pub task_error: Option<String>,
    /// The structured feed, ordered by `seq`.
    pub events: Vec<AgentEventPush>,
    /// The session the daemon resolved for `tasks.events`.
    pub events_session: Option<String>,
    /// The highest seen per-session `seq`.
    pub events_cursor: u64,
    /// Whether the first `tasks.events` backfill is in flight.
    pub events_loading: bool,
    /// The last `tasks.events` failure.
    pub events_error: Option<String>,
    /// Whether at least one backfill completed (an empty feed is then real).
    pub events_loaded: bool,
    /// Event kinds skipped because this client does not know them.
    pub events_ignored: usize,
    /// Monotonic revision of [`TaskDetail::events`], used by the virtualized
    /// feed to rebuild its item list only when the events actually changed.
    pub events_revision: u64,
    /// The attempt table from `tasks.runs`.
    pub runs: Vec<TaskRun>,
    /// Whether the first `tasks.runs` fetch is in flight.
    pub runs_loading: bool,
    /// The last `tasks.runs` failure.
    pub runs_error: Option<String>,
    /// Whether at least one run fetch completed.
    pub runs_loaded: bool,
    /// The active tab.
    pub tab: DetailTab,
    /// Whether new content should keep the viewport at the tail.
    pub follow: bool,
    /// Events that arrived while the viewport was pinned off the tail.
    pub new_events: u64,
    /// The request id with an in-flight `agents.reply`.
    pub reply_pending: Option<String>,
    /// The edited form for the current elicitation request.
    pub elicitation: Option<crate::elicitation::ElicitationFormState>,
    /// The elicitation request id with an in-flight `agents.reply_elicitation`.
    pub elicitation_pending: Option<String>,
    /// The elicitation id most recently answered, hidden until a new one arrives.
    pub answered_elicitation: Option<String>,
    /// The live session id whose turn has an in-flight/cancelling
    /// `agents.cancel`. Cleared when the turn reports idle or the RPC fails.
    pub turn_cancel_pending: Option<String>,
    /// The request id most recently answered, hidden until a new request arrives.
    pub answered_request: Option<String>,
    /// The current execution plan snapshot from `tasks.events`/live updates.
    pub plan: Option<Vec<AgentPlanEntry>>,
    /// The current advertised command snapshot.
    pub commands: Option<Vec<AgentCommand>>,
    /// The follow-up prompt draft.
    pub prompt: String,
    /// The highlighted command completion in the prompt.
    pub prompt_selected: Option<usize>,
    /// Whether an `agents.prompt` is in flight.
    pub prompt_sending: bool,
    /// Whether the follow-up composer is open and owns the keyboard.
    pub prompt_open: bool,
    /// The last prompt-send failure.
    pub prompt_error: Option<String>,
}

impl TaskDetail {
    /// A fresh detail whose three reads are about to start.
    pub fn new(task_id: Uuid) -> Self {
        Self {
            task_id,
            task: None,
            task_loading: true,
            task_error: None,
            events: Vec::new(),
            events_session: None,
            events_cursor: 0,
            events_loading: true,
            events_error: None,
            events_loaded: false,
            events_ignored: 0,
            events_revision: 0,
            runs: Vec::new(),
            runs_loading: true,
            runs_error: None,
            runs_loaded: false,
            tab: DetailTab::default(),
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
            prompt: String::new(),
            prompt_selected: None,
            prompt_sending: false,
            prompt_open: false,
            prompt_error: None,
        }
    }

    /// Mark the three reads in flight again without discarding loaded data, so a
    /// retried/refreshed tab keeps its rows visible (no flicker).
    pub fn reload(&mut self) {
        self.task_loading = true;
        self.events_loading = true;
        self.runs_loading = true;
        self.task_error = None;
        self.events_error = None;
        self.runs_error = None;
    }

    /// Fold a full `tasks.get` result in.
    pub fn set_task(&mut self, task: Task) {
        self.task = Some(task);
        self.task_loading = false;
        self.task_error = None;
    }

    /// Fold a compact `task.updated`/`tasks.list` row in, preserving the output
    /// blob that only `tasks.get` carries. A new attempt starts a new feed, so
    /// the prior attempt's events and output are cleared instead of mixing.
    pub fn update_summary(&mut self, mut task: Task) {
        let attempt_changed = self
            .task
            .as_ref()
            .is_some_and(|existing| existing.attempt != task.attempt);
        if attempt_changed {
            self.events.clear();
            self.events_cursor = 0;
            self.events_loaded = false;
            self.events_ignored = 0;
            self.events_revision = self.events_revision.wrapping_add(1);
            self.new_events = 0;
            self.runs.clear();
            self.runs_loaded = false;
            self.answered_request = None;
            self.reply_pending = None;
            self.turn_cancel_pending = None;
            self.plan = None;
            self.commands = None;
            self.prompt.clear();
            self.prompt_selected = None;
            self.prompt_sending = false;
            self.prompt_open = false;
            self.prompt_error = None;
        } else if task.output.is_none() {
            if let Some(existing) = &self.task {
                task.output = existing.output.clone();
            }
        }
        self.task = Some(task);
        self.task_loading = false;
    }

    /// Fold a `tasks.get` failure in; loaded data is kept.
    pub fn fail_task(&mut self, message: String) {
        self.task_loading = false;
        self.task_error = Some(message);
    }

    /// Merge a `tasks.events` backfill with any live events already received.
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
            highest = highest.max(event.seq);
            if latest_session.is_none() {
                latest_session = Some(event.session_id.clone());
            }
            merged.insert((event.session_id.clone(), event.seq), event);
        }
        let mut events: Vec<AgentEventPush> = merged.into_values().collect();
        events.sort_by_key(|event| (event.seq, event.session_id.clone()));

        // The snapshot is the state at `next_cursor`. Replay only later live
        // events so a plan clearing event trimmed from the retained feed cannot
        // be undone by an older plan event that is still in the window. When an
        // older daemon omits the snapshot, derive it from the whole window.
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
        // An older daemon omits the snapshot fields; derive them from the whole
        // retained window instead.
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

    /// Fold `tasks.events` failure in; loaded events are kept.
    pub fn fail_events(&mut self, message: String) {
        self.events_loading = false;
        self.events_error = Some(message);
    }

    /// Append one live event, deduplicating by `(session_id, seq)`. Returns
    /// whether it was new; a new event while the viewport is pinned off the tail
    /// increments [`TaskDetail::new_events`].
    pub fn push_event(&mut self, push: AgentEventPush) -> bool {
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
        // The daemon sends live events in order, but a reconnect can replay a
        // frame; sorting keeps the feed ordered by `seq` either way.
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

    /// Keep the retained feed bounded. The oldest frames go first; an open
    /// permission prompt is the newest unresolved request, so it survives.
    fn trim_events(&mut self) {
        let excess = self.events.len().saturating_sub(MAX_RETAINED_EVENTS);
        if excess > 0 {
            self.events.drain(..excess);
        }
    }

    /// Fold a `tasks.runs` result in.
    pub fn set_runs(&mut self, runs: Vec<TaskRun>) {
        self.runs = runs;
        self.runs_loading = false;
        self.runs_error = None;
        self.runs_loaded = true;
    }

    /// Fold a `tasks.runs` failure in; loaded rows are kept.
    pub fn fail_runs(&mut self, message: String) {
        self.runs_loading = false;
        self.runs_error = Some(message);
    }

    /// The persisted output, reduced to plain text.
    pub fn output(&self) -> Option<OutputText> {
        extract_output(self.task.as_ref().and_then(|task| task.output.as_ref()))
    }

    /// The grouped event feed.
    pub fn feed(&self) -> Vec<FeedItem> {
        build_feed(&self.events)
    }

    /// The task status, when known.
    pub fn status(&self) -> Option<TaskStatus> {
        self.task.as_ref().map(|task| task.status)
    }

    /// Whether the run is finished and will not change again.
    pub fn is_terminal(&self) -> bool {
        self.status().is_some_and(tasks::is_terminal)
    }

    /// The latest unresolved structured prompt in the event feed.
    fn latest_unresolved_request(&self) -> Option<(&str, &InputRequest)> {
        let mut pending: Option<(&str, &InputRequest)> = None;
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
    /// The live `agents.list` session is authoritative: it carries the request's
    /// exact correlation id, options and `allow_always`. When the session roster
    /// has not caught up yet, the last unresolved `InputRequested` event in the
    /// feed is the fallback. A screen-detected prompt with no structured request
    /// still gets a banner (the run is blocked), but its buttons are disabled
    /// because `request_id` is absent.
    pub fn pending_request(&self, sessions: &[AgentSessionInfo]) -> Option<PendingRequest> {
        if self.is_terminal() {
            return None;
        }
        let task_id = self.task_id.to_string();

        if let Some(session) = sessions
            .iter()
            .find(|session| session.task_id.as_deref() == Some(task_id.as_str()))
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

        // The task says it is blocked but neither the roster nor the feed has
        // the prompt (yet). Say so instead of hiding the block.
        if self.status() == Some(TaskStatus::AwaitingInput) {
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
    fn latest_unresolved_elicitation(&self) -> Option<(&str, &ElicitationRequest)> {
        let mut pending: Option<(&str, &ElicitationRequest)> = None;
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
    /// falling back to the feed. The agent identity comes from the request.
    pub fn pending_elicitation(&self, sessions: &[AgentSessionInfo]) -> Option<PendingElicitation> {
        if self.is_terminal() {
            return None;
        }
        let task_id = self.task_id.to_string();
        if let Some(session) = sessions
            .iter()
            .find(|session| session.task_id.as_deref() == Some(task_id.as_str()))
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
                        request: ElicitationRequest {
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

    /// The current plan, falling back to the last matching event for payloads
    /// from a daemon that predates the explicit snapshot field.
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
        let rest = self.prompt.strip_prefix('/')?;
        if rest.chars().any(char::is_whitespace) {
            return None;
        }
        Some(rest)
    }

    /// Advertised commands matching the current query, in advertised order.
    pub fn command_matches(&self) -> Vec<&AgentCommand> {
        let Some(query) = self.command_query() else {
            return Vec::new();
        };
        let Some(commands) = &self.commands else {
            return Vec::new();
        };
        commands
            .iter()
            .filter(|command| command.name.starts_with(query))
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
        self.prompt = format!("/{name} ");
        self.prompt_selected = None;
        true
    }

    /// The exact matched command's input hint, when present.
    #[allow(dead_code)] // Kept on the pure model for parity with the TUI; the view reads it inline.
    pub fn command_hint(&self) -> Option<&str> {
        let rest = self.prompt.strip_prefix('/')?;
        let name = rest.split_whitespace().next().unwrap_or_default();
        self.commands
            .as_ref()?
            .iter()
            .find(|command| command.name == name)
            .and_then(|command| command.input_hint.as_deref())
    }
}

/// A `tasks.events` result that skips event kinds this client does not know.
///
/// The typed [`favetto_wire::rpc::TasksEventsResult`] fails to decode the whole
/// backfill when one event kind is newer than the client. The detail surface
/// instead keeps every event it understands and reports how many it skipped,
/// so an upgraded daemon never blanks the feed.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DetailEventsResult {
    /// The resolved session, when the task has a structured feed.
    pub session_id: Option<String>,
    /// The events this client understands, oldest-first.
    pub events: Vec<AgentEventPush>,
    /// The daemon's backfill cursor.
    pub next_cursor: u64,
    /// The session's current plan at `next_cursor`, when the daemon sent one.
    pub plan: Option<Vec<AgentPlanEntry>>,
    /// The session's current advertised commands at `next_cursor`.
    pub commands: Option<Vec<AgentCommand>>,
    /// How many events were skipped because their kind is unknown.
    pub ignored: usize,
}

impl<'de> Deserialize<'de> for DetailEventsResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            session_id: Option<String>,
            #[serde(default)]
            events: Vec<serde_json::Value>,
            #[serde(default)]
            next_cursor: u64,
            #[serde(default)]
            plan: Option<Vec<AgentPlanEntry>>,
            #[serde(default)]
            commands: Option<Vec<AgentCommand>>,
        }

        let raw = Raw::deserialize(deserializer)?;
        let mut events = Vec::with_capacity(raw.events.len());
        let mut ignored = 0;
        for value in raw.events {
            match serde_json::from_value::<AgentEventPush>(value) {
                Ok(event) => events.push(event),
                Err(_) => ignored += 1,
            }
        }
        Ok(Self {
            session_id: raw.session_id,
            events,
            next_cursor: raw.next_cursor,
            plan: raw.plan,
            commands: raw.commands,
            ignored,
        })
    }
}

/// The typed call for a tolerant `tasks.events` backfill.
pub struct DetailEventsCall;

impl RpcCall for DetailEventsCall {
    const METHOD: &'static str = favetto_wire::rpc::method::TASKS_EVENTS;
    type Params = TasksEventsParams;
    type Result = DetailEventsResult;
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn push(seq: u64, event: AgentStateEvent) -> AgentEventPush {
        AgentEventPush {
            session_id: "ses_1".to_string(),
            task_id: None,
            seq,
            event,
        }
    }

    fn text(seq: u64, role: MessageRole, text: &str) -> AgentEventPush {
        push(
            seq,
            AgentStateEvent::TextDelta {
                role,
                text: text.to_string(),
            },
        )
    }

    fn task(status: TaskStatus) -> Task {
        Task {
            id: Uuid::from_u128(7),
            name: "repo:favetto/triage".to_string(),
            status,
            attempt: 1,
            input: serde_json::json!({}),
            output: None,
            dedupe_key: None,
            created_at: Utc::now(),
            started_at: Some(Utc::now()),
            finished_at: None,
            error: None,
            failure: None,
            session_id: None,
            session_title: None,
            parent_id: None,
            root_id: None,
            interactive: false,
        }
    }

    #[test]
    fn extract_output_reads_the_envelope_and_flags_truncation() {
        let value = serde_json::json!({ "output": "hello\nworld", "truncated": true });
        let output = extract_output(Some(&value)).expect("output");
        assert_eq!(output.text, "hello\nworld");
        assert!(output.truncated);

        let bare = serde_json::json!("plain");
        let output = extract_output(Some(&bare)).expect("output");
        assert_eq!(output.text, "plain");
        assert!(!output.truncated);

        // An object without `output` is pretty-printed, not dropped.
        let structured = serde_json::json!({ "result": { "ok": true } });
        let output = extract_output(Some(&structured)).expect("output");
        assert!(output.text.contains("\"result\""));

        assert!(extract_output(None).is_none());
        assert!(extract_output(Some(&serde_json::Value::Null)).is_none());
        assert!(extract_output(Some(&serde_json::json!({ "output": "" }))).is_none());
        // A truncated empty body still earns the notice.
        let empty = serde_json::json!({ "output": "", "truncated": true });
        assert!(extract_output(Some(&empty)).expect("output").truncated);
    }

    #[test]
    fn strip_ansi_removes_colour_and_cursor_sequences() {
        let input = "\u{1b}[31mred\u{1b}[0m \u{1b}]0;title\u{07}plain";
        assert_eq!(strip_ansi(input), "red plain");
    }

    #[test]
    fn usage_summary_names_cost_and_tokens() {
        let usage = AgentUsage {
            input_tokens: 1_200,
            output_tokens: 300,
            cost_usd: Some(0.05),
            ..Default::default()
        };
        let summary = usage_summary(Some(&usage)).expect("summary");
        assert!(summary.contains("$0.0500"));
        assert!(summary.contains("1.5k tok"));
        assert!(usage_summary(None).is_none());
        assert!(usage_summary(Some(&AgentUsage::default())).is_none());

        let credits = AgentUsage {
            cost: Some(favetto_wire::model::AgentCost::new(2.5, "credits")),
            ..Default::default()
        };
        let summary = usage_summary(Some(&credits)).expect("summary");
        assert!(summary.contains("2.5000 credits"));
        assert!(!summary.contains('$'));
    }

    #[test]
    fn a_new_detail_has_no_pending_turn_interrupt() {
        let detail = TaskDetail::new(Uuid::from_u128(7));
        assert!(detail.turn_cancel_pending.is_none());
        assert!(detail.reply_pending.is_none());
    }

    #[test]
    fn feed_groups_consecutive_deltas_and_merges_tools() {
        let events = vec![
            text(1, MessageRole::Assistant, "hel"),
            text(2, MessageRole::Assistant, "lo"),
            text(3, MessageRole::User, "hi"),
            push(
                4,
                AgentStateEvent::ReasoningDelta {
                    text: "why ".into(),
                },
            ),
            push(5, AgentStateEvent::ReasoningDelta { text: "not".into() }),
            push(
                6,
                AgentStateEvent::ToolStarted {
                    id: "c1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({ "command": "ls" }),
                    detail: None,
                },
            ),
            push(
                7,
                AgentStateEvent::ToolFinished {
                    id: "c1".into(),
                    name: "bash".into(),
                    ok: true,
                    output: Some(serde_json::json!("a.txt")),
                    detail: None,
                },
            ),
            push(
                8,
                AgentStateEvent::Usage {
                    usage: AgentUsage {
                        input_tokens: 2,
                        ..Default::default()
                    },
                    snapshot: false,
                },
            ),
            push(
                9,
                AgentStateEvent::Idle {
                    outcome: IdleOutcome::Succeeded,
                },
            ),
            push(
                10,
                AgentStateEvent::Error {
                    message: "boom".into(),
                },
            ),
        ];
        let feed = build_feed(&events);
        assert!(matches!(
            &feed[0],
            FeedItem::Message { role: MessageRole::Assistant, text } if text == "hello"
        ));
        assert!(matches!(
            &feed[1],
            FeedItem::Message { role: MessageRole::User, text } if text == "hi"
        ));
        assert!(matches!(
            &feed[2],
            FeedItem::Reasoning { text, streaming: false } if text == "why not"
        ));
        match &feed[3] {
            FeedItem::Tool { status, output, .. } => {
                assert_eq!(*status, ToolStatus::Ok);
                assert_eq!(output, &Some(serde_json::json!("a.txt")));
            }
            other => panic!("expected a tool card, got {other:?}"),
        }
        assert!(matches!(&feed[4], FeedItem::Usage(_)));
        assert!(matches!(
            &feed[5],
            FeedItem::Boundary(IdleOutcome::Succeeded)
        ));
        assert!(matches!(&feed[6], FeedItem::Error(message) if message == "boom"));
    }

    #[test]
    fn feed_keeps_non_text_content_blocks_in_order() {
        let content = MessageContent::Image {
            mime_type: "image/png".to_string(),
            uri: Some("https://example.com/a.png".to_string()),
        };
        let events = vec![
            text(1, MessageRole::Assistant, "before"),
            push(
                2,
                AgentStateEvent::ContentBlock {
                    role: ContentRole::Assistant,
                    content: content.clone(),
                },
            ),
            text(3, MessageRole::Assistant, "after"),
        ];
        let feed = build_feed(&events);
        assert_eq!(feed.len(), 3);
        assert!(matches!(
            &feed[0],
            FeedItem::Message { text, .. } if text == "before"
        ));
        assert!(matches!(
            &feed[1],
            FeedItem::ContentBlock { role: ContentRole::Assistant, content: block }
                if block == &content
        ));
        assert!(matches!(
            &feed[2],
            FeedItem::Message { text, .. } if text == "after"
        ));
        assert!(content.placeholder().contains("image/png"));
        assert!(content.placeholder().contains("https://example.com/a.png"));
    }

    #[test]
    fn auth_progress_stays_in_the_banner_not_the_transcript() {
        let auth = favetto_wire::model::AgentAuthState {
            status: favetto_wire::model::AgentAuthStatus::Required,
            methods: vec![favetto_wire::model::AgentAuthMethod {
                id: "agent-login".to_string(),
                name: "Sign in".to_string(),
                description: None,
                kind: favetto_wire::model::AgentAuthMethodKind::Agent,
                supported: true,
            }],
            logout_supported: true,
            method_id: None,
            message: Some("the agent requires sign-in".to_string()),
        };
        let feed = build_feed(&[push(1, AgentStateEvent::Auth { auth })]);
        assert!(feed.is_empty(), "auth state must not duplicate feed rows");
    }

    #[test]
    fn compact_json_skips_empty_values_and_cuts_long_ones() {
        assert!(compact_json(&serde_json::Value::Null).is_none());
        assert!(compact_json(&serde_json::json!({})).is_none());
        assert!(compact_json(&serde_json::json!([])).is_none());
        assert_eq!(
            compact_json(&serde_json::json!({ "a": 1 })).as_deref(),
            Some("{\"a\":1}")
        );
        let long = "x".repeat(500);
        let cut = compact_json(&serde_json::json!(long)).expect("cut");
        assert!(cut.ends_with('…'));
        assert!(cut.chars().count() <= 241);
    }

    #[test]
    fn push_event_dedupes_and_counts_only_while_pinned() {
        let mut detail = TaskDetail::new(Uuid::from_u128(7));
        detail.set_task(task(TaskStatus::Running));
        assert!(detail.push_event(text(2, MessageRole::Assistant, "two")));
        assert!(!detail.push_event(text(2, MessageRole::Assistant, "two")));
        assert_eq!(detail.events.len(), 1);
        // Following: no unseen counter.
        assert_eq!(detail.new_events, 0);

        detail.follow = false;
        assert!(detail.push_event(text(1, MessageRole::Assistant, "one")));
        assert_eq!(detail.events.len(), 2);
        assert_eq!(detail.events[0].seq, 1, "events stay ordered by seq");
        assert_eq!(detail.new_events, 1);
    }

    #[test]
    fn the_live_feed_is_capped_and_keeps_the_newest_frames() {
        let mut detail = TaskDetail::new(Uuid::from_u128(7));
        for seq in 0..(MAX_RETAINED_EVENTS as u64 + 25) {
            detail.push_event(text(seq, MessageRole::Assistant, "line"));
        }
        assert_eq!(detail.events.len(), MAX_RETAINED_EVENTS);
        assert_eq!(detail.events.first().map(|event| event.seq), Some(25));
        assert_eq!(
            detail.events.last().map(|event| event.seq),
            Some(MAX_RETAINED_EVENTS as u64 + 24)
        );
    }

    #[test]
    fn merge_events_prefers_backfill_and_keeps_live_gaps() {
        let mut detail = TaskDetail::new(Uuid::from_u128(7));
        detail.push_event(text(3, MessageRole::Assistant, "live"));
        detail.merge_events(
            Some("ses_1".to_string()),
            vec![
                text(1, MessageRole::Assistant, "one"),
                text(3, MessageRole::Assistant, "durable"),
            ],
            3,
            None,
            None,
            2,
        );
        assert_eq!(detail.events.len(), 2);
        assert_eq!(
            detail.events[1].event,
            AgentStateEvent::TextDelta {
                role: MessageRole::Assistant,
                text: "durable".to_string()
            }
        );
        assert_eq!(detail.events_cursor, 3);
        assert_eq!(detail.events_ignored, 2);
        assert!(detail.events_loaded);
        assert!(detail.events_error.is_none());
    }

    #[test]
    fn merge_events_applies_the_snapshot_then_only_later_live_events() {
        let mut detail = TaskDetail::new(Uuid::from_u128(7));
        let old = push(
            2,
            AgentStateEvent::Plan {
                entries: vec![AgentPlanEntry {
                    content: "old".to_string(),
                    ..Default::default()
                }],
            },
        );
        // The daemon's snapshot is already cleared at next_cursor, so an older
        // plan event still in the retained window must not resurrect it.
        detail.push_event(push(
            10,
            AgentStateEvent::TextDelta {
                role: MessageRole::Assistant,
                text: "after".to_string(),
            },
        ));
        detail.merge_events(
            Some("ses_1".to_string()),
            vec![old.clone()],
            8,
            Some(Vec::new()),
            Some(Vec::new()),
            0,
        );
        assert!(detail.plan().is_empty());

        // A later live plan (seq > next_cursor) beats the older snapshot.
        let mut detail = TaskDetail::new(Uuid::from_u128(7));
        detail.push_event(old);
        detail.push_event(push(
            12,
            AgentStateEvent::Plan {
                entries: vec![AgentPlanEntry {
                    content: "live".to_string(),
                    ..Default::default()
                }],
            },
        ));
        detail.merge_events(
            Some("ses_1".to_string()),
            Vec::new(),
            8,
            Some(vec![AgentPlanEntry {
                content: "snapshot".to_string(),
                ..Default::default()
            }]),
            None,
            0,
        );
        assert_eq!(detail.plan()[0].content, "live");
    }

    #[test]
    fn plan_and_command_updates_replace_whole_lists() {
        let mut detail = TaskDetail::new(Uuid::from_u128(7));
        detail.push_event(push(
            1,
            AgentStateEvent::Plan {
                entries: vec![AgentPlanEntry {
                    content: "first".to_string(),
                    ..Default::default()
                }],
            },
        ));
        detail.push_event(push(
            2,
            AgentStateEvent::Plan {
                entries: vec![
                    AgentPlanEntry {
                        content: "second".to_string(),
                        ..Default::default()
                    },
                    AgentPlanEntry {
                        content: "third".to_string(),
                        ..Default::default()
                    },
                ],
            },
        ));
        assert_eq!(detail.plan().len(), 2);
        assert_eq!(detail.plan()[0].content, "second");

        detail.push_event(push(
            3,
            AgentStateEvent::AvailableCommands {
                commands: vec![AgentCommand {
                    name: "web".to_string(),
                    description: "Search".to_string(),
                    input_hint: Some("query".to_string()),
                }],
            },
        ));
        assert_eq!(detail.commands().len(), 1);
        detail.push_event(push(
            4,
            AgentStateEvent::AvailableCommands {
                commands: Vec::new(),
            },
        ));
        assert!(detail.commands().is_empty(), "an empty list clears");
    }

    #[test]
    fn command_completion_is_explicit_and_never_rewrites_unknown_text() {
        let mut detail = TaskDetail::new(Uuid::from_u128(7));
        detail.commands = Some(vec![
            AgentCommand {
                name: "web".to_string(),
                description: "Search".to_string(),
                input_hint: Some("query".to_string()),
            },
            AgentCommand {
                name: "test".to_string(),
                description: "Run tests".to_string(),
                input_hint: None,
            },
        ]);

        detail.prompt = "/w".to_string();
        assert_eq!(detail.command_query(), Some("w"));
        assert_eq!(detail.command_matches().len(), 1);
        assert!(detail.complete_command());
        assert_eq!(detail.prompt, "/web ");
        assert_eq!(detail.command_hint(), Some("query"));

        detail.prompt = "/nope arg".to_string();
        assert!(!detail.complete_command());
        assert_eq!(
            detail.prompt, "/nope arg",
            "unknown commands stay ordinary prompt text"
        );

        detail.prompt = "/t".to_string();
        assert!(detail.complete_command());
        assert_eq!(detail.prompt, "/test ");
        detail.prompt.push('x');
        detail.prompt_selected = Some(0);
        detail.prompt.pop();
        assert_eq!(detail.prompt, "/test ");
    }

    #[test]
    fn pending_request_uses_the_session_then_the_feed() {
        let mut detail = TaskDetail::new(Uuid::from_u128(7));
        detail.set_task(task(TaskStatus::AwaitingInput));
        let request = InputRequest {
            id: "perm_1".to_string(),
            kind: AwaitingInputKind::Permission,
            message: "Allow?".to_string(),
            options: vec!["yes".to_string()],
            allow_always: true,
            form: None,
        };
        let session = AgentSessionInfo {
            id: "ses_1".to_string(),
            agent: "opencode".to_string(),
            backend: Default::default(),
            task_id: Some(Uuid::from_u128(7).to_string()),
            running: true,
            headless: true,
            session_id: None,
            awaiting_input: Some(favetto_wire::model::AwaitingInputReason {
                kind: request.kind,
                message: request.message.clone(),
                request_id: Some(request.id.clone()),
                options: request.options.clone(),
                allow_always: request.allow_always,
                form: None,
            }),
            activity: None,
            usage: None,
            load_state: Default::default(),
            load_error: None,

            config_options: Vec::new(),
            modes: None,
            auth: None,
        };
        let pending = detail
            .pending_request(std::slice::from_ref(&session))
            .expect("pending");
        assert_eq!(pending.request_id.as_deref(), Some("perm_1"));
        assert!(pending.can_reply());
        assert!(pending.allow_always);

        // Answered: hidden until a new request id arrives.
        detail.answered_request = Some("perm_1".to_string());
        assert!(detail
            .pending_request(std::slice::from_ref(&session))
            .is_none());

        // A screen-detected prompt (no request id) is display-only.
        let mut no_id = session.clone();
        no_id.awaiting_input.as_mut().unwrap().request_id = None;
        detail.answered_request = None;
        let pending = detail.pending_request(&[no_id]).expect("pending");
        assert!(!pending.can_reply());

        // Without the roster, the feed's unresolved request is the fallback.
        let mut detail = TaskDetail::new(Uuid::from_u128(7));
        detail.set_task(task(TaskStatus::Running));
        detail.push_event(push(1, AgentStateEvent::InputRequested { request }));
        let pending = detail.pending_request(&[]).expect("pending");
        assert_eq!(pending.session_id, "ses_1");
        assert_eq!(pending.request_id.as_deref(), Some("perm_1"));
        detail.push_event(push(
            2,
            AgentStateEvent::InputResolved {
                id: "perm_1".to_string(),
            },
        ));
        assert!(detail.pending_request(&[]).is_none());
    }

    #[test]
    fn terminal_or_unknown_blocked_runs_still_explain_themselves() {
        let mut detail = TaskDetail::new(Uuid::from_u128(7));
        detail.set_task(task(TaskStatus::Succeeded));
        assert!(detail.pending_request(&[]).is_none());

        let mut detail = TaskDetail::new(Uuid::from_u128(7));
        detail.set_task(task(TaskStatus::AwaitingInput));
        let pending = detail.pending_request(&[]).expect("generic banner");
        assert!(!pending.can_reply());
        assert!(pending.message.contains("waiting for input"));
    }

    #[test]
    fn a_new_attempt_clears_the_previous_feed_and_output() {
        let mut detail = TaskDetail::new(Uuid::from_u128(7));
        let mut first = task(TaskStatus::Failed);
        first.output = Some(serde_json::json!({ "output": "old" }));
        detail.set_task(first);
        detail.push_event(text(1, MessageRole::Assistant, "old"));
        assert!(!detail.events.is_empty());

        let mut second = task(TaskStatus::Running);
        second.attempt = 2;
        detail.update_summary(second);
        assert!(detail.events.is_empty());
        assert!(!detail.events_loaded);
        assert!(detail.output().is_none());
        assert_eq!(detail.status(), Some(TaskStatus::Running));

        // A same-attempt update keeps the output blob and feed.
        let mut same = task(TaskStatus::Running);
        same.output = None;
        let mut detail = TaskDetail::new(Uuid::from_u128(7));
        let mut loaded = task(TaskStatus::Running);
        loaded.output = Some(serde_json::json!({ "output": "kept" }));
        detail.set_task(loaded);
        detail.push_event(text(1, MessageRole::Assistant, "live"));
        detail.update_summary(same);
        assert_eq!(
            detail.output().map(|output| output.text),
            Some("kept".to_string())
        );
        assert_eq!(detail.events.len(), 1);
    }

    #[test]
    fn tolerant_backfill_skips_unknown_event_kinds() {
        let value = serde_json::json!({
            "session_id": "ses_1",
            "next_cursor": 9,
            "events": [
                { "session_id": "ses_1", "seq": 1, "event": { "event": "turn_started" } },
                { "session_id": "ses_1", "seq": 2, "event": { "event": "future_kind", "x": 1 } },
                { "session_id": "ses_1", "seq": 3, "event": { "event": "error", "message": "boom" } }
            ]
        });
        let result: DetailEventsResult = serde_json::from_value(value).expect("decode");
        assert_eq!(result.events.len(), 2);
        assert_eq!(result.ignored, 1);
        assert_eq!(result.next_cursor, 9);
        assert_eq!(result.session_id.as_deref(), Some("ses_1"));
    }

    #[test]
    fn elicitation_feed_and_pending_follow_the_matching_request_id() {
        let request = ElicitationRequest {
            id: "acp-elic-1".to_string(),
            agent: "codex".to_string(),
            tool_call_id: None,
            message: "Fill in".to_string(),
            form: favetto_wire::elicitation::ElicitationForm {
                title: Some("Details".to_string()),
                description: None,
                fields: Vec::new(),
            },
        };
        let mut detail = TaskDetail::new(Uuid::from_u128(7));
        detail.set_task(task(TaskStatus::AwaitingInput));
        detail.push_event(push(
            1,
            AgentStateEvent::ElicitationRequested {
                request: request.clone(),
            },
        ));
        let feed = build_feed(&detail.events);
        assert!(matches!(
            &feed[0],
            FeedItem::Elicitation { request: shown, resolved: false } if shown.id == "acp-elic-1"
        ));
        let pending = detail
            .pending_elicitation(&[])
            .expect("feed fallback pending");
        assert_eq!(pending.request, request);
        detail.sync_elicitation(&[]);
        assert_eq!(
            detail
                .elicitation
                .as_ref()
                .map(|form| form.request.id.as_str()),
            Some("acp-elic-1")
        );

        // A different id must not clear the open form.
        detail.push_event(push(
            2,
            AgentStateEvent::ElicitationResolved {
                id: "other".to_string(),
                outcome: favetto_wire::elicitation::ElicitationOutcome::Cancelled,
            },
        ));
        detail.sync_elicitation(&[]);
        assert!(detail.elicitation.is_some());
        assert!(matches!(
            &build_feed(&detail.events)[0],
            FeedItem::Elicitation {
                resolved: false,
                ..
            }
        ));

        detail.push_event(push(
            3,
            AgentStateEvent::ElicitationResolved {
                id: "acp-elic-1".to_string(),
                outcome: favetto_wire::elicitation::ElicitationOutcome::Declined,
            },
        ));
        detail.sync_elicitation(&[]);
        assert!(detail.elicitation.is_none());
        assert!(detail.pending_elicitation(&[]).is_none());
        assert!(matches!(
            &build_feed(&detail.events)[0],
            FeedItem::Elicitation { resolved: true, .. }
        ));
    }

    #[test]
    fn run_helpers_name_the_run_only_outcomes() {
        let started = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        let run = TaskRun {
            id: Uuid::from_u128(1),
            task_id: Uuid::from_u128(7),
            attempt: 2,
            status: RunStatus::TimedOut,
            agent: None,
            model: None,
            session_id: None,
            started_at: Some(started),
            finished_at: Some(started + chrono::Duration::seconds(90)),
            exit_code: Some(1),
            error: None,
            usage: None,
            failure: None,
        };
        assert_eq!(run_status_label(run.status), "timed out");
        assert_eq!(run_status_kind(run.status), StatusKind::Neutral);
        assert_eq!(run_duration(&run, started), Some(Duration::from_secs(90)));
        assert_eq!(run_exit_label(&run), "1");
    }
}
