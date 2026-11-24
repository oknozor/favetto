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
use std::ops::{Deref, DerefMut};
use std::time::Duration;

use favetto_wire::elicitation::ElicitationRequest;
use favetto_wire::model::{
    AgentCommand, AgentPlanEntry, AgentSessionHistoryEntry, AgentSessionState, AgentStateEvent,
    AgentUsage, AwaitingInputKind, ContentRole, IdleOutcome, InputRequest, MessageContent,
    MessageRole, RunStatus, RunSummary, Task, TaskRun, TaskStatus,
};
use favetto_wire::rpc::{AgentAttachResult, AgentEventPush, RpcCall, TasksEventsParams};
use favetto_wire::Timestamp;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::conversation::Conversation;
#[cfg(test)]
use favetto_wire::model::AgentSessionInfo;

use crate::inbox::{self, StatusKind};
use crate::search::{self, SearchField, SearchQuery};
use crate::tasks;

/// The maximum number of event frames retained for one open detail. The daemon
/// already caps the backfill at 2000; this keeps a long-running live feed from
/// growing without bound while the window stays open.
pub const MAX_RETAINED_EVENTS: usize = 2_000;

/// The two tabs of the detail surface (docs §2): one merged output/feed view
/// plus the attempt table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DetailTab {
    /// The merged output view: the live feed while running, the durable
    /// `RunSummary` once terminal, or the plain persisted blob as a fallback.
    #[default]
    Output,
    /// The `tasks.runs` attempt table.
    Runs,
}

impl DetailTab {
    /// Every tab, in bar order.
    pub const ALL: [DetailTab; 2] = [DetailTab::Output, DetailTab::Runs];

    /// The tab label.
    pub fn label(self) -> &'static str {
        match self {
            DetailTab::Output => "Output",
            DetailTab::Runs => "Runs",
        }
    }
}

/// Where the merged output tab gets its rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputSource {
    /// The ephemeral live `agent.event` feed, while the task is non-terminal.
    Live,
    /// The durable `RunSummary` decoded from `task.output`.
    Summary,
    /// No structured source: the plain persisted output.
    Plain,
}

/// Pick the merged output tab's source.
///
/// While a task is non-terminal a live feed wins; once terminal the persisted
/// summary wins. A terminal task that still has a cached live feed but no
/// summary yet keeps showing the feed, so the tab never blanks during the
/// transition.
pub fn output_source(status: TaskStatus, has_live: bool, has_summary: bool) -> OutputSource {
    if has_summary && (tasks::is_terminal(status) || !has_live) {
        OutputSource::Summary
    } else if has_live {
        OutputSource::Live
    } else if has_summary {
        OutputSource::Summary
    } else {
        OutputSource::Plain
    }
}

/// Extract the structured [`RunSummary`] stored in a `tasks.get` output value.
///
/// The executor stores it under `result`; a run that reported nothing stores
/// `null`, and a legacy result may be a different shape. An empty summary is
/// treated as absent so the caller keeps the plain-text fallback.
pub fn extract_run_summary(value: &serde_json::Value) -> Option<RunSummary> {
    let candidate = value
        .get("result")
        .filter(|result| !result.is_null())
        .unwrap_or(value);
    let summary: RunSummary = serde_json::from_value(candidate.clone()).ok()?;
    (!summary.is_empty()).then_some(summary)
}

/// Map a persisted [`RunSummary`] onto the feed view model, so a finished run's
/// log renders after a reload or daemon restart when the live feed is gone.
///
/// The summary is already coalesced, so this emits a canonical order — session,
/// message, reasoning, tool cards, usage, error, run boundary — matching how a
/// coding-agent transcript reads top to bottom.
pub fn summary_feed(summary: &RunSummary) -> Vec<FeedItem> {
    let mut items = Vec::new();
    if let Some(title) = summary
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
    {
        items.push(FeedItem::Session {
            title: Some(title.to_string()),
        });
    }
    if !summary.text.trim().is_empty() {
        items.push(FeedItem::Message {
            role: MessageRole::Assistant,
            text: summary.text.clone(),
        });
    }
    if !summary.reasoning.trim().is_empty() {
        items.push(FeedItem::Reasoning {
            text: summary.reasoning.clone(),
            streaming: false,
        });
    }
    for call in &summary.tool_calls {
        items.push(FeedItem::Tool {
            id: call.id.clone(),
            name: call.name.clone(),
            input: call.input.clone(),
            status: if call.ok {
                ToolStatus::Ok
            } else {
                ToolStatus::Error
            },
            output: call.output.clone(),
        });
    }
    if !summary.usage.is_empty() {
        items.push(FeedItem::Usage(summary.usage.clone()));
    }
    if let Some(error) = &summary.error {
        items.push(FeedItem::Error(error.clone()));
    }
    if let Some(outcome) = summary.outcome {
        items.push(FeedItem::Boundary(outcome));
    }
    items
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
///
/// Unknown values are omitted rather than fabricated: a run with tokens but no
/// reported cost shows the token totals, and a context-only `usage_update` shows
/// its context window even when it also reported `$0`. A genuine reported zero
/// is still rendered as `$0.0000`.
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
    } else if let Some(used) = usage.context_used {
        parts.push(format_context(used, usage.context_size));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The compact `ctx used/size` label for context-only usage.
pub fn format_context(used: u64, size: Option<u64>) -> String {
    let used = format_tokens(used).replace(" tok", "");
    match size {
        Some(size) => format!("ctx {used}/{}", format_tokens(size).replace(" tok", "")),
        None => format!("ctx {used}"),
    }
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

/// Whether a grouped feed contains text worth reading as a conversation.
///
/// Only [`FeedItem::Message`] and [`FeedItem::Reasoning`] carry the agent's
/// prose. Session/title rows, tool cards, usage, errors, boundaries, forms and
/// content-block placeholders can fill the feed without replaying what the
/// agent actually said, so a restored transcript must still be shown above
/// them.
pub fn feed_has_readable_text(items: &[FeedItem]) -> bool {
    items.iter().any(|item| match item {
        FeedItem::Message { text, .. } | FeedItem::Reasoning { text, .. } => {
            !text.trim().is_empty()
        }
        _ => false,
    })
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
pub fn run_duration(run: &TaskRun, now: Timestamp) -> Option<Duration> {
    let started = run.started_at?;
    let end = run.finished_at.unwrap_or(now);
    let seconds = end
        .unix_timestamp()
        .saturating_sub(started.unix_timestamp());
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

/// How much of a restored conversation the detail renders. The agent owns the
/// full transcript; this is a bounded, read-only preview that keeps its tail so
/// the current response stays visible.
pub const RESTORED_TRANSCRIPT_CHARS: usize = 8_000;

/// The task-scoped restoration of the run's associated agent conversation.
///
/// The state machine is GPUI-free: the daemon lookup (`tasks.session`) and a
/// single `agents.load` are the only reads, and neither ever starts a fresh
/// session or prompts the agent.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionRestore {
    /// `tasks.session` is in flight.
    Checking,
    /// The daemon resolved an associated session and `agents.load` is replaying
    /// its conversation.
    Attaching {
        session_id: String,
        agent: String,
        /// Whether the resolved session was live when the lookup ran.
        live: bool,
    },
    /// The conversation was restored and can be rendered read-only.
    Restored(RestoredSession),
    /// The task has no associated session, or it cannot be restored.
    Unavailable(String),
    /// The lookup, load, or transcript decode failed.
    Failed(String),
}

/// One restored agent conversation, addressed by its Favetto-local session id.
#[derive(Clone, Debug, PartialEq)]
pub struct RestoredSession {
    /// Favetto-local id, the only routing key accepted by `agents.load`.
    pub session_id: String,
    /// Configured agent that owns the session.
    pub agent: String,
    /// The session lifecycle at replay time.
    pub state: AgentSessionState,
    /// The last normalized-event sequence already folded into `transcript`.
    ///
    /// The live feed resumes strictly after this cursor, so the restored
    /// transcript and the structured rows never paint the same text twice.
    pub seq: u64,
    /// The replayed plain-text conversation, bounded for display.
    pub transcript: String,
}

impl RestoredSession {
    /// Whether the replay contains anything worth painting.
    pub fn has_transcript(&self) -> bool {
        !self.transcript.trim().is_empty()
    }

    /// The lifecycle as a short label.
    pub fn state_label(&self) -> &'static str {
        match self.state {
            AgentSessionState::Active => "live",
            AgentSessionState::Loading => "loading",
            AgentSessionState::Ended => "ended",
            AgentSessionState::Failed => "failed",
        }
    }
}

/// Decode an `agents.load` replay into a bounded display transcript.
pub fn restored_session(
    attached: AgentAttachResult,
    expected_id: &str,
) -> Result<RestoredSession, String> {
    if attached.session.id != expected_id {
        return Err(format!(
            "the daemon loaded session '{}' instead of '{expected_id}'",
            attached.session.id
        ));
    }
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&attached.data)
        .map_err(|_| "the daemon returned an unreadable session transcript".to_string())?;
    let transcript = String::from_utf8(bytes)
        .map_err(|_| "the daemon returned a transcript that is not valid UTF-8".to_string())?;
    let transcript = strip_ansi(&transcript);
    let state = if attached.session.running {
        AgentSessionState::Active
    } else {
        AgentSessionState::Ended
    };
    Ok(RestoredSession {
        session_id: attached.session.id,
        agent: attached.session.agent,
        state,
        seq: attached.seq,
        transcript: bound_transcript(&transcript),
    })
}

fn bound_transcript(transcript: &str) -> String {
    let count = transcript.chars().count();
    if count <= RESTORED_TRANSCRIPT_CHARS {
        return transcript.to_string();
    }
    let skip = count - RESTORED_TRANSCRIPT_CHARS;
    let tail: String = transcript.chars().skip(skip).collect();
    format!("… (earlier transcript truncated for display)\n{tail}")
}

/// The client's per-task detail state. One lives on [`crate::state::ClientState`]
/// for the selected run and is replaced when another run opens.
///
/// Everything task-neutral lives in [`Conversation`]; this wrapper keeps only
/// the task row, its attempt table, the active tab and the task-scoped restore.
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
    /// The shared structured conversation.
    pub conversation: Conversation,
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
    /// The task-scoped restoration of the run's agent conversation.
    pub restore: SessionRestore,
    /// Monotonic id of the in-flight restore lookup, used to drop stale replies.
    pub restore_request: u64,
    /// The Favetto-local id this detail asked `agents.load` for, when attaching.
    pub restore_session: Option<String>,
}

impl Deref for TaskDetail {
    type Target = Conversation;

    fn deref(&self) -> &Self::Target {
        &self.conversation
    }
}

impl DerefMut for TaskDetail {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.conversation
    }
}

impl TaskDetail {
    /// A fresh detail whose three reads are about to start.
    pub fn new(task_id: Uuid) -> Self {
        let mut conversation = Conversation::for_task(task_id);
        conversation.events_loading = true;
        Self {
            task_id,
            task: None,
            task_loading: true,
            task_error: None,
            conversation,
            runs: Vec::new(),
            runs_loading: true,
            runs_error: None,
            runs_loaded: false,
            tab: DetailTab::default(),
            restore: SessionRestore::Checking,
            restore_request: 0,
            restore_session: None,
        }
    }

    /// Mark the three reads in flight again without discarding loaded data, so a
    /// retried/refreshed tab keeps its rows visible (no flicker).
    pub fn reload(&mut self) {
        self.task_loading = true;
        self.conversation.events_loading = true;
        self.runs_loading = true;
        self.task_error = None;
        self.conversation.events_error = None;
        self.runs_error = None;
    }

    /// Fold a full `tasks.get` result in.
    pub fn set_task(&mut self, task: Task) {
        self.conversation.mark_status(
            tasks::is_terminal(task.status),
            task.status == TaskStatus::AwaitingInput,
        );
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
            self.conversation.reset_for_new_attempt();
            self.runs.clear();
            self.runs_loaded = false;
            // A new attempt owns a new agent conversation; the previous attempt's
            // restored history must never appear under it.
            self.restore = SessionRestore::Checking;
            self.restore_request = self.restore_request.wrapping_add(1);
            self.restore_session = None;
        } else if task.output.is_none() {
            if let Some(existing) = &self.task {
                task.output = existing.output.clone();
            }
        }
        self.conversation.mark_status(
            tasks::is_terminal(task.status),
            task.status == TaskStatus::AwaitingInput,
        );
        self.task = Some(task);
        self.task_loading = false;
    }

    /// Fold a `tasks.get` failure in; loaded data is kept.
    pub fn fail_task(&mut self, message: String) {
        self.task_loading = false;
        self.task_error = Some(message);
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

    /// Start a fresh, task-scoped restore lookup and return its generation id.
    ///
    /// The generation is echoed by the runtime so a reply for an earlier open or
    /// attempt can never overwrite the current detail.
    pub fn begin_restore(&mut self) -> u64 {
        self.restore_request = self.restore_request.wrapping_add(1);
        self.restore = SessionRestore::Checking;
        self.restore_session = None;
        self.restore_request
    }

    /// Record that the resolved session is being restored through `agents.load`.
    pub fn attach_restore(&mut self, entry: &AgentSessionHistoryEntry) {
        self.restore_session = Some(entry.id.clone());
        self.restore = SessionRestore::Attaching {
            session_id: entry.id.clone(),
            agent: entry.agent.clone(),
            live: entry.state == AgentSessionState::Active,
        };
    }

    /// Whether a `tasks.session` reply belongs to this detail's current open or
    /// attempt. A duplicate reply after the attach started is not accepted.
    pub fn accepts_restore_reply(&self, task_id: Uuid, request_id: u64) -> bool {
        self.task_id == task_id
            && self.restore_request == request_id
            && matches!(self.restore, SessionRestore::Checking)
    }

    /// Record that the task has no restorable associated session.
    pub fn mark_restore_unavailable(&mut self, reason: String) {
        self.restore_session = None;
        self.restore = SessionRestore::Unavailable(reason);
    }

    /// Record a lookup/load failure; the task row and its output are untouched.
    pub fn fail_restore(&mut self, reason: String) {
        self.restore_session = None;
        self.restore = SessionRestore::Failed(reason);
    }

    /// Fold a completed `agents.load` in.
    ///
    /// Returns the feed cursor to reload from when the restored session changed,
    /// so the caller backfills only rows newer than the transcript snapshot.
    pub fn finish_restore(&mut self, result: Result<AgentAttachResult, String>) -> Option<u64> {
        let expected = self.restore_session.clone()?;
        match result.and_then(|attached| restored_session(attached, &expected)) {
            Ok(restored) => {
                let changed = self.events_session.as_deref() != Some(restored.session_id.as_str());
                let seq = restored.seq;
                if changed {
                    // Drop the previous attempt's feed so a replayed
                    // conversation is not filtered out by the old cursor.
                    self.conversation.reset_events_for_restore();
                }
                // Rows already folded into the transcript must not reappear in
                // the live feed, including an in-flight backfill from zero.
                self.conversation.restore_events_after(seq);
                self.restore = SessionRestore::Restored(restored);
                self.restore_session = None;
                changed.then_some(seq)
            }
            Err(message) => {
                self.fail_restore(message);
                None
            }
        }
    }

    /// The persisted output, reduced to plain text.
    pub fn output(&self) -> Option<OutputText> {
        extract_output(self.task.as_ref().and_then(|task| task.output.as_ref()))
    }

    /// The task status, when known.
    pub fn status(&self) -> Option<TaskStatus> {
        self.task.as_ref().map(|task| task.status)
    }

    /// Whether the run is finished and will not change again.
    pub fn is_terminal(&self) -> bool {
        self.status().is_some_and(tasks::is_terminal)
    }
}

/// The `/prefix` currently being completed, only for a bare first token.
pub fn command_query(prompt: &str) -> Option<&str> {
    let rest = prompt.strip_prefix('/')?;
    if rest.chars().any(char::is_whitespace) {
        return None;
    }
    Some(rest)
}

/// Advertised commands matching a draft prompt, most relevant first and
/// advertised order as the tie-break. The index is the command's position in
/// the advertised list, so a click still selects it after the list is ranked.
pub fn matching_commands<'a>(
    commands: &'a [AgentCommand],
    prompt: &str,
) -> Vec<(usize, &'a AgentCommand)> {
    let Some(query_text) = command_query(prompt) else {
        return Vec::new();
    };
    rank_commands(commands, query_text)
}

/// Rank commands for an already extracted `/prefix` query.
fn rank_commands<'a>(
    commands: &'a [AgentCommand],
    query_text: &str,
) -> Vec<(usize, &'a AgentCommand)> {
    let query = SearchQuery::new(query_text);
    let matcher = search::matcher();
    let mut scored: Vec<(i64, usize, &AgentCommand)> = commands
        .iter()
        .enumerate()
        .filter_map(|(index, command)| {
            let fields = [SearchField::new(&command.name, search::weight::LABEL)];
            query
                .score(&matcher, &fields)
                .map(|matched| (matched.score, index, command))
        })
        .collect();
    scored.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
    scored
        .into_iter()
        .map(|(_, index, command)| (index, command))
        .collect()
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
            created_at: Timestamp::now(),
            started_at: Some(Timestamp::now()),
            finished_at: None,
            error: None,
            failure: None,
            session_id: None,
            session_title: None,
            parent_id: None,
            root_id: None,
            interactive: false,
            agent: None,
            provider: None,
            model: None,
            latest_usage: None,
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
    fn extract_run_summary_reads_the_result_and_rejects_empty_stubs() {
        let value = serde_json::json!({
            "agent": "opencode",
            "output": "raw jsonl",
            "result": {
                "session_id": "ses_1",
                "title": "Fix it",
                "text": "done",
                "reasoning": "because",
                "tool_calls": [{ "id": "c1", "name": "read", "ok": true }],
                "usage": { "input_tokens": 3 },
                "outcome": "succeeded",
            },
        });
        let summary = extract_run_summary(&value).expect("structured summary");
        assert_eq!(summary.text, "done");
        assert_eq!(summary.tool_calls.len(), 1);
        assert_eq!(summary.outcome, Some(IdleOutcome::Succeeded));

        // A bare summary is accepted too.
        let bare = serde_json::json!({ "text": "hello" });
        assert_eq!(extract_run_summary(&bare).unwrap().text, "hello");

        // `result: null`, an oversized stub, and a title-only summary are not
        // renderable logs, so the plain fallback stays.
        assert!(extract_run_summary(&serde_json::json!({
            "output": "raw",
            "result": serde_json::Value::Null,
        }))
        .is_none());
        assert!(extract_run_summary(&serde_json::json!({
            "result": { "preview": "…", "truncated": true },
        }))
        .is_none());
        assert!(extract_run_summary(&serde_json::json!({
            "result": { "title": "Fix it" },
        }))
        .is_none());
    }

    #[test]
    fn summary_feed_emits_the_canonical_durable_log_order() {
        let summary = RunSummary {
            session_id: Some("ses_1".to_string()),
            title: Some("Fix it".to_string()),
            text: "done".to_string(),
            reasoning: "because".to_string(),
            tool_calls: vec![favetto_wire::model::ToolCall {
                id: "c1".to_string(),
                name: "bash".to_string(),
                input: serde_json::json!({ "command": "ls" }),
                ok: true,
                output: Some(serde_json::json!("a.txt")),
                detail: None,
            }],
            usage: AgentUsage {
                input_tokens: 2,
                ..Default::default()
            },
            outcome: Some(IdleOutcome::Succeeded),
            error: Some("note".to_string()),
        };
        let feed = summary_feed(&summary);
        assert!(matches!(&feed[0], FeedItem::Session { title: Some(title) } if title == "Fix it"));
        assert!(matches!(
            &feed[1],
            FeedItem::Message { role: MessageRole::Assistant, text } if text == "done"
        ));
        assert!(matches!(
            &feed[2],
            FeedItem::Reasoning { text, streaming: false } if text == "because"
        ));
        assert!(matches!(
            &feed[3],
            FeedItem::Tool { name, status: ToolStatus::Ok, .. } if name == "bash"
        ));
        assert!(matches!(&feed[4], FeedItem::Usage(_)));
        assert!(matches!(&feed[5], FeedItem::Error(message) if message == "note"));
        assert!(matches!(
            &feed[6],
            FeedItem::Boundary(IdleOutcome::Succeeded)
        ));
        assert!(summary_feed(&RunSummary::default()).is_empty());
    }

    #[test]
    fn output_source_switches_live_to_summary_without_blanking_the_transition() {
        // Non-terminal: a live feed wins; without one, the durable summary.
        for status in [
            TaskStatus::Pending,
            TaskStatus::Running,
            TaskStatus::AwaitingInput,
        ] {
            assert_eq!(output_source(status, true, false), OutputSource::Live);
            assert_eq!(output_source(status, true, true), OutputSource::Live);
            assert_eq!(output_source(status, false, true), OutputSource::Summary);
        }

        // Terminal: the persisted summary wins even with a cached feed.
        for status in [
            TaskStatus::Succeeded,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ] {
            assert_eq!(output_source(status, true, true), OutputSource::Summary);
            assert_eq!(output_source(status, false, true), OutputSource::Summary);
            // Transition: cached feed, summary not decoded yet -> keep the feed.
            assert_eq!(output_source(status, true, false), OutputSource::Live);
        }

        // Nothing structured: the plain persisted output.
        assert_eq!(
            output_source(TaskStatus::Pending, false, false),
            OutputSource::Plain
        );
        assert_eq!(
            output_source(TaskStatus::Succeeded, false, false),
            OutputSource::Plain
        );
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

        // Tokens without a cost show the tokens and never a fabricated `$0`.
        let tokens_only = AgentUsage {
            input_tokens: 900,
            output_tokens: 100,
            ..Default::default()
        };
        let summary = usage_summary(Some(&tokens_only)).expect("summary");
        assert!(summary.contains("1k tok"));
        assert!(!summary.contains('$'));

        // Context-only usage renders as context even when a zero cost is also
        // reported; a reported zero is still zero, not unknown.
        let context_only = AgentUsage {
            context_used: Some(53_000),
            context_size: Some(200_000),
            cost: Some(favetto_wire::model::AgentCost::usd(0.0)),
            ..Default::default()
        };
        let summary = usage_summary(Some(&context_only)).expect("summary");
        assert!(summary.contains("$0.0000"));
        assert!(summary.contains("ctx 53k/200k"));
        assert_eq!(format_context(53_000, Some(200_000)), "ctx 53k/200k");
        assert_eq!(format_context(53_000, None), "ctx 53k");

        let unknown = AgentUsage {
            context_used: Some(53_000),
            ..Default::default()
        };
        let summary = usage_summary(Some(&unknown)).expect("summary");
        assert!(summary.contains("ctx 53k"));
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

        detail.prompt.set_value("/w".to_string());
        assert_eq!(detail.command_query(), Some("w"));
        assert_eq!(detail.command_matches().len(), 1);
        assert!(detail.complete_command());
        assert_eq!(detail.prompt, "/web ");
        assert_eq!(detail.command_hint(), Some("query"));

        detail.prompt.set_value("/nope arg".to_string());
        assert!(!detail.complete_command());
        assert_eq!(
            detail.prompt, "/nope arg",
            "unknown commands stay ordinary prompt text"
        );

        detail.prompt.set_value("/t".to_string());
        assert!(detail.complete_command());
        assert_eq!(detail.prompt, "/test ");
        detail.prompt.push_char('x');
        detail.prompt_selected = Some(0);
        detail.prompt.backspace();
        assert_eq!(detail.prompt, "/test ");
    }

    #[test]
    fn slash_completion_is_case_insensitive() {
        let mut detail = TaskDetail::new(Uuid::from_u128(7));
        detail.commands = Some(vec![AgentCommand {
            name: "web".to_string(),
            description: "Search".to_string(),
            input_hint: Some("query".to_string()),
        }]);

        detail.prompt.set_value("/W".to_string());
        assert_eq!(detail.command_matches().len(), 1);
        assert!(detail.complete_command());
        assert_eq!(detail.prompt, "/web ");
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
            mcp: None,
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
        let started = "2026-01-01T12:00:00Z".parse::<Timestamp>().unwrap();
        let run = TaskRun {
            id: Uuid::from_u128(1),
            task_id: Uuid::from_u128(7),
            attempt: 2,
            status: RunStatus::TimedOut,
            agent: None,
            model: None,
            session_id: None,
            started_at: Some(started),
            finished_at: Some(started + time::Duration::seconds(90)),
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

    fn history_entry(
        id: &str,
        state: AgentSessionState,
        loadable: bool,
    ) -> AgentSessionHistoryEntry {
        AgentSessionHistoryEntry {
            id: id.to_string(),
            agent: "opencode".to_string(),
            title: Some(format!("session {id}")),
            cwd: std::path::PathBuf::from("/work"),
            task_id: Some(Uuid::from_u128(7).to_string()),
            state,
            loadable,
            load_error: None,
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
        }
    }

    fn attached(id: &str, text: &str) -> AgentAttachResult {
        attached_state(id, text, false, 0)
    }

    fn attached_state(id: &str, text: &str, running: bool, seq: u64) -> AgentAttachResult {
        AgentAttachResult {
            session: AgentSessionInfo {
                id: id.to_string(),
                agent: "opencode".to_string(),
                backend: favetto_wire::model::AgentBackend::Acp,
                task_id: Some(Uuid::from_u128(7).to_string()),
                running,
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
            },
            data: base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                text.as_bytes(),
            ),
            seq,
            plan: Vec::new(),
            commands: Vec::new(),
        }
    }

    #[test]
    fn restore_attaches_a_resolved_session_and_decodes_the_replay() {
        let mut detail = TaskDetail::new(Uuid::from_u128(7));
        let request = detail.begin_restore();
        assert!(matches!(detail.restore, SessionRestore::Checking));
        assert_eq!(request, 1);

        detail.attach_restore(&history_entry("local-past", AgentSessionState::Ended, true));
        assert_eq!(detail.restore_session.as_deref(), Some("local-past"));

        let reload = detail.finish_restore(Ok(attached("local-past", "question\nhello")));
        assert_eq!(reload, Some(0), "there was no prior feed session");
        let SessionRestore::Restored(restored) = &detail.restore else {
            panic!("expected a restored session: {:?}", detail.restore);
        };
        assert_eq!(restored.session_id, "local-past");
        assert_eq!(restored.agent, "opencode");
        assert_eq!(restored.state, AgentSessionState::Ended);
        assert_eq!(restored.seq, 0);
        assert_eq!(restored.transcript, "question\nhello");
        assert!(detail.restore_session.is_none());
    }

    #[test]
    fn a_stale_or_duplicate_restore_reply_is_dropped() {
        let task_id = Uuid::from_u128(7);
        let mut detail = TaskDetail::new(task_id);
        let request = detail.begin_restore();
        assert!(detail.accepts_restore_reply(task_id, request));

        // A reply for another task or an earlier attempt is stale.
        assert!(!detail.accepts_restore_reply(Uuid::from_u128(8), request));
        assert!(!detail.accepts_restore_reply(task_id, request + 1));

        // Once the attach started, a duplicate lookup reply is a no-op.
        detail.attach_restore(&history_entry("local-past", AgentSessionState::Ended, true));
        assert!(!detail.accepts_restore_reply(task_id, request));
    }

    #[test]
    fn restore_records_unavailable_and_failed_states_without_falling_back() {
        let mut detail = TaskDetail::new(Uuid::from_u128(7));
        detail.begin_restore();
        detail.attach_restore(&history_entry(
            "local-failed",
            AgentSessionState::Ended,
            false,
        ));
        detail.finish_restore(Err("agent does not support session/load".to_string()));
        assert!(matches!(
            detail.restore,
            SessionRestore::Failed(ref message) if message.contains("does not support")
        ));
        assert!(detail.restore_session.is_none());

        detail.begin_restore();
        detail
            .mark_restore_unavailable("No agent conversation is associated with this task.".into());
        assert!(matches!(
            detail.restore,
            SessionRestore::Unavailable(ref reason) if reason.contains("No agent conversation")
        ));
    }

    #[test]
    fn restore_drops_a_replay_for_a_different_local_id() {
        let mut detail = TaskDetail::new(Uuid::from_u128(7));
        detail.begin_restore();
        detail.attach_restore(&history_entry("local-past", AgentSessionState::Ended, true));
        detail.finish_restore(Ok(attached("provider-id", "wrong transcript")));
        assert!(matches!(
            detail.restore,
            SessionRestore::Failed(ref message) if message.contains("provider-id")
        ));
        assert!(detail.restore_session.is_none());
    }

    #[test]
    fn restore_resets_the_feed_cursor_when_the_session_changes() {
        let mut detail = TaskDetail::new(Uuid::from_u128(7));
        detail.push_event(text(4, MessageRole::Assistant, "old"));
        assert_eq!(detail.events_session.as_deref(), Some("ses_1"));
        assert_eq!(detail.events_cursor, 4);

        detail.begin_restore();
        detail.attach_restore(&history_entry("local-past", AgentSessionState::Ended, true));
        let reload = detail.finish_restore(Ok(attached("local-past", "replay")));
        assert_eq!(reload, Some(0));
        assert!(detail.events.is_empty());
        assert!(detail.events_session.is_none());
        assert_eq!(detail.events_cursor, 0);
        assert_eq!(detail.events_floor, 0);
        assert!(!detail.events_loaded);
    }

    #[test]
    fn restore_keeps_a_live_sessions_attach_cursor_and_drops_replayed_rows() {
        let mut detail = TaskDetail::new(Uuid::from_u128(7));
        detail.push_event(text(3, MessageRole::Assistant, "already in transcript"));
        detail.push_event(text(8, MessageRole::Assistant, "newer"));

        detail.begin_restore();
        detail.attach_restore(&history_entry("ses_1", AgentSessionState::Active, true));
        let reload = detail.finish_restore(Ok(attached_state(
            "ses_1",
            "question\nassistant answer",
            true,
            5,
        )));

        // The same session is already attached: no second backfill is needed.
        assert_eq!(reload, None);
        let SessionRestore::Restored(restored) = &detail.restore else {
            panic!("expected a restored session");
        };
        assert_eq!(restored.seq, 5);
        assert_eq!(restored.state, AgentSessionState::Active);
        assert!(restored.has_transcript());
        // Rows covered by the transcript are dropped; newer rows survive.
        assert_eq!(
            detail
                .events
                .iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            vec![8]
        );
        assert_eq!(detail.events_floor, 5);
        assert_eq!(detail.events_cursor, 8);

        // Late/in-flight backfill rows below the snapshot are deduplicated too.
        detail.merge_events(
            Some("ses_1".to_string()),
            vec![
                text(4, MessageRole::Assistant, "replayed"),
                text(9, MessageRole::Assistant, "live"),
            ],
            9,
            None,
            None,
            0,
        );
        assert_eq!(
            detail
                .events
                .iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            vec![8, 9]
        );
        assert!(!detail.push_event(text(5, MessageRole::Assistant, "stale live")));
        assert!(detail.push_event(text(10, MessageRole::Assistant, "next")));
    }

    #[test]
    fn restore_backfills_from_the_attach_seq_when_the_session_changes() {
        let mut detail = TaskDetail::new(Uuid::from_u128(7));
        detail.push_event(text(4, MessageRole::Assistant, "previous session"));

        detail.begin_restore();
        detail.attach_restore(&history_entry(
            "local-past",
            AgentSessionState::Active,
            true,
        ));
        let reload = detail.finish_restore(Ok(attached_state("local-past", "replay", true, 12)));

        assert_eq!(reload, Some(12));
        assert!(detail.events.is_empty());
        assert_eq!(detail.events_cursor, 12);
        assert_eq!(detail.events_floor, 12);
    }

    #[test]
    fn a_new_attempt_clears_the_restore_floor() {
        let mut detail = TaskDetail::new(Uuid::from_u128(7));
        detail.set_task(task(TaskStatus::Running));
        detail.begin_restore();
        detail.attach_restore(&history_entry("ses_1", AgentSessionState::Active, true));
        detail.finish_restore(Ok(attached_state("ses_1", "old", true, 9)));
        assert_eq!(detail.events_floor, 9);

        let mut retry = task(TaskStatus::Running);
        retry.attempt = 2;
        detail.update_summary(retry);
        assert_eq!(detail.events_floor, 0);
        assert!(detail.push_event(text(3, MessageRole::Assistant, "fresh")));
    }

    #[test]
    fn only_message_and_reasoning_rows_are_readable_feed_text() {
        assert!(!feed_has_readable_text(&[]));

        // Lifecycle, title-only session rows, tools and usage are not prose.
        let sparse = build_feed(&[
            push(1, AgentStateEvent::TurnStarted),
            push(
                2,
                AgentStateEvent::Session {
                    session_id: Some("ses_1".into()),
                    title: Some("Fix it".into()),
                    model: None,
                },
            ),
            push(
                3,
                AgentStateEvent::ToolStarted {
                    id: "c1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({ "command": "ls" }),
                    detail: None,
                },
            ),
            push(
                4,
                AgentStateEvent::Usage {
                    usage: AgentUsage::default(),
                    snapshot: false,
                },
            ),
        ]);
        assert!(!feed_has_readable_text(&sparse));

        let plan = build_feed(&[push(
            5,
            AgentStateEvent::Plan {
                entries: vec![AgentPlanEntry {
                    content: "inspect".into(),
                    ..Default::default()
                }],
            },
        )]);
        assert!(!feed_has_readable_text(&plan));

        // Whitespace-only deltas do not count either.
        let blank = build_feed(&[text(6, MessageRole::Assistant, "  \n ")]);
        assert!(!feed_has_readable_text(&blank));

        let message = build_feed(&[text(7, MessageRole::Assistant, "answer")]);
        assert!(feed_has_readable_text(&message));
        let reasoning = build_feed(&[push(
            8,
            AgentStateEvent::ReasoningDelta { text: "why".into() },
        )]);
        assert!(feed_has_readable_text(&reasoning));
    }

    #[test]
    fn restore_does_not_duplicate_text_already_in_the_transcript() {
        let mut detail = TaskDetail::new(Uuid::from_u128(7));
        detail.push_event(text(3, MessageRole::Assistant, "assistant answer"));

        detail.begin_restore();
        detail.attach_restore(&history_entry("ses_1", AgentSessionState::Active, true));
        detail.finish_restore(Ok(attached_state(
            "ses_1",
            "question\nassistant answer",
            true,
            3,
        )));
        assert!(detail.events.is_empty(), "replayed rows are deduplicated");

        // New live text still lands in the feed after the transcript snapshot.
        assert!(detail.push_event(text(4, MessageRole::Assistant, "next answer")));
        let feed = build_feed(&detail.events);
        assert!(feed_has_readable_text(&feed));
        let SessionRestore::Restored(restored) = &detail.restore else {
            panic!("expected a restored session");
        };
        assert!(restored.transcript.contains("assistant answer"));
        assert_eq!(
            feed.iter()
                .filter_map(|item| match item {
                    FeedItem::Message { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec!["next answer"]
        );
    }

    #[test]
    fn the_restored_transcript_keeps_its_tail() {
        let head = "old ".repeat(RESTORED_TRANSCRIPT_CHARS);
        let latest = "LATEST RESPONSE";
        let mut detail = TaskDetail::new(Uuid::from_u128(7));
        detail.begin_restore();
        detail.attach_restore(&history_entry("local-past", AgentSessionState::Ended, true));
        detail.finish_restore(Ok(attached("local-past", &format!("{head}{latest}"))));
        let SessionRestore::Restored(restored) = &detail.restore else {
            panic!("expected a restored session");
        };
        assert!(restored.transcript.starts_with('…'));
        assert!(restored.transcript.ends_with(latest));
        assert!(restored.transcript.contains("truncated for display"));
        assert!(
            restored.transcript.chars().count() <= RESTORED_TRANSCRIPT_CHARS + 64,
            "the transcript preview stays bounded"
        );
    }
}
