//! Normalized, wire-serializable agent state shared by the daemon and TUI.
//!
//! Each external-agent CLI speaks a different machine-readable dialect. These
//! types are the one vocabulary every adapter normalizes to, so consumers (the
//! executor, the Agent panel, `agents.list`) never branch on the CLI. See
//! `docs/design/agent-state-adapters.md` §3.
//!
//! All types are additive on the wire: every new field defaults, so an older
//! client or daemon decodes a newer payload unchanged.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use std::str::FromStr;

use crate::model::{AwaitingInputKind, MessageRole};

/// What an agent is doing right now (coarse, for the task list / picker).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentActivity {
    /// The process is starting up.
    Starting,
    /// The model is thinking (no visible output yet).
    Thinking,
    /// The model is streaming a response.
    Responding,
    /// A tool is running.
    Tool {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    /// Blocked on the user.
    Waiting { request: InputRequest },
    /// Alive but between turns.
    Idle,
    /// The process has exited (interactive TUI remains attachable).
    Exited { code: Option<i32> },
}

/// A prompt the agent is blocked on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct InputRequest {
    /// Transport-specific correlation id (permission id, extension-ui id, …).
    pub id: String,
    pub kind: AwaitingInputKind,
    /// Human-readable prompt text.
    pub message: String,
    /// Selectable options; empty means free-form text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// Whether an "always" / "remember" answer is offered.
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_always: bool,
}

/// The answer to an [`InputRequest`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum InputReply {
    /// Allow this one occurrence.
    Once,
    /// Allow and remember.
    Always,
    /// Reject the request.
    Reject,
    /// Free-form value (pinentry, text input).
    Value { value: String },
    /// Confirmation answer.
    Confirmed { confirmed: bool },
    /// The prompt was dismissed.
    Cancelled,
}

/// A cost amount with the unit the agent reported it in.
///
/// ACP's `cost.currency` is an ISO 4217 code for money (`"USD"`, `"EUR"`, …) or
/// another unit such as `"credits"`. Keeping the unit alongside the amount is
/// what stops a non-USD value from being rendered as dollars.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentCost {
    pub amount: f64,
    /// ISO 4217 currency code or another agent-defined unit.
    pub currency: String,
}

impl AgentCost {
    /// The ISO code treated as the legacy dollar amount.
    pub const USD: &'static str = "USD";

    pub fn new(amount: f64, currency: impl Into<String>) -> Self {
        Self {
            amount,
            currency: currency.into(),
        }
    }

    /// A dollar amount, for adapters and legacy records that only report USD.
    pub fn usd(amount: f64) -> Self {
        Self::new(amount, Self::USD)
    }

    /// Whether this amount is denominated in USD.
    pub fn is_usd(&self) -> bool {
        self.currency.eq_ignore_ascii_case(Self::USD)
    }

    /// Add a same-unit amount. Returns `false` without changing `self` when the
    /// units differ, because there is no exchange rate to apply.
    pub fn merge(&mut self, other: &AgentCost) -> bool {
        if !self.currency.eq_ignore_ascii_case(&other.currency) {
            return false;
        }
        self.amount += other.amount;
        true
    }

    /// Render the amount with an explicit unit: `$0.0123` for USD and
    /// `2.5000 credits` (or `1.5000 EUR`) for anything else.
    pub fn display(&self) -> String {
        if !self.amount.is_finite() {
            return "—".to_string();
        }
        if self.is_usd() {
            format!("${:.4}", self.amount)
        } else {
            let unit = self.currency.trim();
            if unit.is_empty() {
                format!("{:.4}", self.amount)
            } else {
                format!("{:.4} {unit}", self.amount)
            }
        }
    }
}

/// Token/cost usage reported by an agent for a turn or a whole run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    /// Legacy USD-only cost field. New payloads also carry [`Self::cost`], and
    /// this field stays populated for USD amounts so older clients and stored
    /// records keep rendering correctly. It is `None` for non-USD amounts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Currency-qualified cost, when the agent reports one. Preferred over
    /// [`Self::cost_usd`] because it preserves non-USD units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<AgentCost>,
    /// Tokens currently in the model's context, when the agent reports a context
    /// window rather than per-step token counts (ACP's `usage_update`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_used: Option<u64>,
    /// The model's total context window size, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_size: Option<u64>,
}

impl AgentUsage {
    /// Fold `other` into `self` (per-step usage summed into a run total).
    ///
    /// This is the additive path: use [`Self::replace`] for an agent snapshot
    /// whose values are already cumulative.
    pub fn merge(&mut self, other: &AgentUsage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.reasoning_tokens += other.reasoning_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
        if let Some(other_cost) = other.reported_cost() {
            match self.reported_cost() {
                None => self.set_cost(Some(other_cost)),
                Some(mut cost) => {
                    if cost.merge(&other_cost) {
                        self.set_cost(Some(cost));
                    }
                    // Different units cannot be summed; keep the known cost.
                }
            }
        }
        // A context snapshot is state, not a per-step total: keep the latest.
        if let Some(used) = other.context_used {
            self.context_used = Some(used);
        }
        if let Some(size) = other.context_size {
            self.context_size = Some(size);
        }
    }

    /// Replace this usage with a cumulative agent snapshot.
    pub fn replace(&mut self, other: &AgentUsage) {
        *self = other.clone();
    }

    /// The cost reported by the agent, preferring the currency-aware field and
    /// falling back to the legacy USD amount on older payloads.
    pub fn reported_cost(&self) -> Option<AgentCost> {
        self.cost
            .clone()
            .or_else(|| self.cost_usd.map(AgentCost::usd))
    }

    /// Set the reported cost, synchronizing the legacy USD field so payloads
    /// stay readable by older clients. Non-USD amounts clear `cost_usd`.
    pub fn set_cost(&mut self, cost: Option<AgentCost>) {
        self.cost_usd = cost
            .as_ref()
            .filter(|cost| cost.is_usd())
            .map(|cost| cost.amount);
        self.cost = cost;
    }

    /// Whether nothing has been reported (all counters zero, no cost).
    pub fn is_empty(&self) -> bool {
        self.input_tokens == 0
            && self.output_tokens == 0
            && self.reasoning_tokens == 0
            && self.cache_read_tokens == 0
            && self.cache_write_tokens == 0
            && self.cost_usd.is_none()
            && self.cost.is_none()
            && self.context_used.is_none()
            && self.context_size.is_none()
    }

    /// Sum of the token classes that are not subsets of one another. Cache
    /// read/write counters are excluded because a cache read is normally also
    /// counted as input and would otherwise be double-counted.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.reasoning_tokens
    }
}

/// The aggregation window requested from `usage.stats`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UsagePeriod {
    /// Hourly buckets over the last 24 hours.
    #[default]
    Day,
    /// Daily buckets over the last 7 days.
    Week,
    /// Daily buckets over the last 30 days.
    Month,
    /// Monthly buckets over the last 12 months.
    Year,
}

impl UsagePeriod {
    /// Every period, in selector order.
    pub const ALL: [UsagePeriod; 4] = [
        UsagePeriod::Day,
        UsagePeriod::Week,
        UsagePeriod::Month,
        UsagePeriod::Year,
    ];

    /// Stable string form used on the wire and in URLs.
    pub fn as_str(self) -> &'static str {
        match self {
            UsagePeriod::Day => "day",
            UsagePeriod::Week => "week",
            UsagePeriod::Month => "month",
            UsagePeriod::Year => "year",
        }
    }

    /// Human-readable name for the TUI period selector.
    pub fn label(self) -> &'static str {
        match self {
            UsagePeriod::Day => "Day",
            UsagePeriod::Week => "Week",
            UsagePeriod::Month => "Month",
            UsagePeriod::Year => "Year",
        }
    }

    /// What a single bucket represents (chart/label unit).
    pub fn bucket_unit(self) -> &'static str {
        match self {
            UsagePeriod::Day => "hour",
            UsagePeriod::Week | UsagePeriod::Month => "day",
            UsagePeriod::Year => "month",
        }
    }
}

impl FromStr for UsagePeriod {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "day" => UsagePeriod::Day,
            "week" => UsagePeriod::Week,
            "month" => UsagePeriod::Month,
            "year" => UsagePeriod::Year,
            _ => return Err(()),
        })
    }
}

/// One time bucket of aggregated usage in a [`UsageStats`] result.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UsageBucket {
    /// Inclusive bucket start (UTC).
    pub start: DateTime<Utc>,
    /// Exclusive bucket end (UTC).
    pub end: DateTime<Utc>,
    /// Token/cost totals for the bucket. Aggregated cost is USD-only (the
    /// legacy `cost_usd` column); non-USD run costs are not mixed into it.
    pub usage: AgentUsage,
    /// Number of finished runs whose `finished_at` fell in the bucket.
    pub runs: u64,
}

impl UsageBucket {
    /// Total tokens in the bucket (see [`AgentUsage::total_tokens`]).
    pub fn total_tokens(&self) -> u64 {
        self.usage.total_tokens()
    }
}

/// Overall totals for a [`UsageStats`] result: the sum of every bucket.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UsageTotals {
    pub usage: AgentUsage,
    /// Number of finished runs in the whole window.
    pub runs: u64,
}

impl UsageTotals {
    /// Total tokens across the window (see [`AgentUsage::total_tokens`]).
    pub fn total_tokens(&self) -> u64 {
        self.usage.total_tokens()
    }
}

/// `usage.stats` result: a per-bucket token/cost series plus window totals for
/// the requested [`UsagePeriod`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UsageStats {
    pub period: UsagePeriod,
    pub totals: UsageTotals,
    /// Oldest bucket first. Empty buckets are included so the TUI chart always
    /// has a fixed number of points for the selected period.
    pub buckets: Vec<UsageBucket>,
}

/// A completed tool invocation, kept in a [`RunSummary`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Correlation id from the agent (e.g. opencode's `callID`).
    pub id: String,
    /// Tool name (e.g. `bash`, `read`).
    pub name: String,
    #[serde(default)]
    pub input: serde_json::Value,
    /// Whether the tool finished successfully.
    #[serde(default)]
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<serde_json::Value>,
    /// Full tool-call detail, when the adapter reports more than the legacy
    /// id/name/input/output quadruple.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Box<ToolCallDetail>>,
}

/// The normalized category of a tool invocation.
///
/// Mirrors ACP's `ToolKind`. [`ToolKind::Other`] is both the protocol default
/// and the fallback for a kind this client does not understand, so a consumer
/// must treat it as "no specific icon" rather than an error.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    /// Reading files or data.
    Read,
    /// Modifying files or content.
    Edit,
    /// Removing files or data.
    Delete,
    /// Moving or renaming files.
    Move,
    /// Searching for information.
    Search,
    /// Running commands or code.
    Execute,
    /// Internal reasoning or planning.
    Think,
    /// Retrieving external data.
    Fetch,
    /// Switching the current session mode.
    SwitchMode,
    /// Other, unknown, or absent tool kind.
    #[default]
    #[serde(other)]
    Other,
}

/// The normalized lifecycle status of a tool invocation.
///
/// Mirrors ACP's `ToolCallStatus`, which has no cancellation variant: a
/// cancelled turn leaves the last reported status in place unless the agent
/// sends [`ToolCallStatus::Failed`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    /// Waiting on input or approval; the tool has not started running.
    #[default]
    Pending,
    /// Currently running.
    InProgress,
    /// Finished successfully.
    Completed,
    /// Finished with an error.
    Failed,
    /// A status this client does not understand.
    #[serde(other)]
    Unknown,
}

impl ToolCallStatus {
    /// Whether the status is final (`Completed` or `Failed`).
    pub fn is_terminal(self) -> bool {
        matches!(self, ToolCallStatus::Completed | ToolCallStatus::Failed)
    }
}

/// One file location touched by a tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ToolLocation {
    /// File path as reported by the agent.
    pub path: String,
    /// Optional 1-based line number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

/// Structured tool output, without unbounded binary payloads.
///
/// Image, audio, and blob resources keep their metadata but deliberately omit
/// the base64 payload: the CLI renders a placeholder instead of decoding it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolContent {
    /// Plain or Markdown text produced by the tool.
    Text { text: String },
    /// An image result, without its base64 payload.
    Image {
        mime_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uri: Option<String>,
    },
    /// An audio result, without its base64 payload.
    Audio { mime_type: String },
    /// A reference to an external resource.
    ResourceLink {
        uri: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<i64>,
    },
    /// An embedded resource. Binary resources carry `binary: true` and no text.
    Resource {
        uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        binary: bool,
    },
    /// A file modification.
    Diff {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        old_text: Option<String>,
        new_text: String,
    },
    /// A terminal the client can render by id.
    Terminal { terminal_id: String },
}

/// The message-chunk role that carried a non-text [`MessageContent`] block.
///
/// This is deliberately separate from [`MessageRole`]: thoughts are a streaming
/// display concept, not a role in the persisted conversation model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ContentRole {
    /// A user message chunk.
    User,
    /// An assistant message chunk.
    Assistant,
    /// The assistant's chain-of-thought.
    Thought,
}

impl ContentRole {
    /// Stable string form used on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            ContentRole::User => "user",
            ContentRole::Assistant => "assistant",
            ContentRole::Thought => "thought",
        }
    }
}

/// A non-text content block carried by a user, assistant, or thought message.
///
/// Binary payloads (base64 image/audio bytes and blob resources) are
/// deliberately omitted. Only the metadata needed to render an explanatory
/// placeholder survives the ACP-to-wire conversion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageContent {
    /// An image, without its base64 payload.
    Image {
        mime_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uri: Option<String>,
    },
    /// An audio clip, without its base64 payload.
    Audio { mime_type: String },
    /// A reference to an external resource.
    ResourceLink {
        uri: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<i64>,
    },
    /// An embedded resource, without its text or binary payload.
    Resource {
        uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        binary: bool,
    },
    /// A block this client does not understand. Clients render a generic
    /// placeholder rather than failing to decode the whole feed.
    #[serde(other)]
    Unknown,
}

impl MessageContent {
    /// A deterministic, plain-text placeholder for clients that cannot render
    /// this block. It never implies a fetch or opens a URI.
    pub fn placeholder(&self) -> String {
        match self {
            MessageContent::Image { mime_type, uri } => {
                media_placeholder("image", mime_type, uri.as_deref())
            }
            MessageContent::Audio { mime_type } => media_placeholder("audio", mime_type, None),
            MessageContent::ResourceLink {
                uri,
                name,
                mime_type,
                size,
                ..
            } => {
                let mut text = format!("link {name} <{uri}>");
                if let Some(mime_type) = mime_type {
                    text.push_str(&format!(" {mime_type}"));
                }
                if let Some(size) = size {
                    text.push_str(&format!(" {size} bytes"));
                }
                text
            }
            MessageContent::Resource {
                uri,
                mime_type,
                binary,
            } => {
                let mut text = format!("resource <{uri}>");
                if let Some(mime_type) = mime_type {
                    text.push_str(&format!(" {mime_type}"));
                }
                if *binary {
                    text.push_str(" (binary)");
                }
                text
            }
            MessageContent::Unknown => "unsupported content block".to_string(),
        }
    }
}

fn media_placeholder(kind: &str, mime_type: &str, uri: Option<&str>) -> String {
    match uri {
        Some(uri) => format!("{kind} {mime_type} <{uri}>"),
        None => format!("{kind} {mime_type}"),
    }
}

/// Cumulative, wire-friendly detail for one tool call.
///
/// This is a *snapshot*, not a patch: every field is the latest value the
/// adapter knows, and consumers replace their previous copy wholesale. ACP
/// agents vary in how much they populate; every field is optional/empty when
/// the agent did not report it, so no consumer may assume a value is present.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ToolCallDetail {
    /// Programmatic tool name, when the agent exposes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Human-readable title (ACP requires one on a start, updates may omit it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Tool category, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ToolKind>,
    /// Lifecycle status, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ToolCallStatus>,
    /// Raw input parameters as reported by the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<serde_json::Value>,
    /// Raw output as reported by the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<serde_json::Value>,
    /// Structured output blocks (text, diffs, terminals, resource metadata).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<ToolContent>,
    /// Files touched by the call.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locations: Vec<ToolLocation>,
}

/// The relative importance of one execution-plan entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AgentPlanPriority {
    High,
    #[default]
    Medium,
    Low,
    /// A priority this client does not understand.
    #[serde(other)]
    Unknown,
}

/// The lifecycle status of one execution-plan entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AgentPlanStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
    /// A status this client does not understand.
    #[serde(other)]
    Unknown,
}

/// One step of the agent's current execution plan.
///
/// A plan update carries the complete ordered checklist; consumers replace their
/// previous list wholesale rather than appending to it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentPlanEntry {
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub priority: AgentPlanPriority,
    #[serde(default)]
    pub status: AgentPlanStatus,
}

/// One slash command advertised by the agent.
///
/// Commands are prompt text: the client inserts `/name ` into the composer and
/// sends the user's prompt unchanged. It never invokes them out of band.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentCommand {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Hint shown for a command that accepts free-form arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_hint: Option<String>,
}

/// How a run or turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum IdleOutcome {
    Succeeded,
    Failed,
    Interrupted,
}

/// One normalized observation about a session, in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AgentStateEvent {
    /// The agent reported (or changed) its own session identity.
    Session {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// A model turn began.
    TurnStarted,
    /// A text block from the assistant.
    TextDelta { role: MessageRole, text: String },
    /// Chain-of-thought text (display-only).
    ReasoningDelta { text: String },
    /// A non-text message block (image, audio, resource, …).
    ///
    /// Text continues to stream through [`AgentStateEvent::TextDelta`] and
    /// [`AgentStateEvent::ReasoningDelta`]. This event preserves the role and
    /// streamed position of every other block without transporting its binary
    /// payload.
    ContentBlock {
        role: ContentRole,
        content: MessageContent,
    },
    /// A tool invocation started.
    ///
    /// `name`/`input` are the legacy flattened fields; `detail` carries the
    /// cumulative, structured snapshot when the adapter has one.
    ToolStarted {
        id: String,
        name: String,
        input: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<Box<ToolCallDetail>>,
    },
    /// Partial tool output (streaming transports).
    ///
    /// `partial` keeps the original metadata/patch payload for older clients;
    /// `detail` is the cumulative snapshot. A sparse update never clears a
    /// field that was already present in `detail`.
    ToolUpdated {
        id: String,
        partial: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<Box<ToolCallDetail>>,
    },
    /// A tool invocation finished.
    ///
    /// `output` stays the legacy raw output; `detail` carries the final
    /// snapshot, including status and structured content.
    ToolFinished {
        id: String,
        name: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<Box<ToolCallDetail>>,
    },
    /// The agent is blocked on the user.
    InputRequested { request: InputRequest },
    /// A previously requested input was answered (or dismissed).
    InputResolved { id: String },
    /// Token/cost usage for a step or a cumulative session snapshot.
    Usage {
        usage: AgentUsage,
        /// When true, `usage` is the agent's current cumulative state (ACP's
        /// `usage_update`) and folds by replacement rather than addition.
        #[serde(default, skip_serializing_if = "is_false")]
        snapshot: bool,
    },
    /// The session's title changed.
    Title { title: String },
    /// The agent's complete current execution plan. Replaces any previous plan;
    /// an empty list is a replacement that clears it.
    Plan {
        #[serde(default)]
        entries: Vec<AgentPlanEntry>,
    },
    /// The agent's complete set of advertised slash commands. Replaces any
    /// previous list; an empty list is a replacement that clears it.
    AvailableCommands {
        #[serde(default)]
        commands: Vec<AgentCommand>,
    },
    /// The session became idle (turn/run boundary).
    Idle { outcome: IdleOutcome },
    /// A session-level error.
    Error { message: String },
}

/// The structured result of a finished run, replacing the ad-hoc output blob.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub reasoning: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub usage: AgentUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<IdleOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl RunSummary {
    /// Whether the summary carries nothing worth rendering. Identity fields
    /// (`session_id`, `title`) do not count: a title-only summary has no log.
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
            && self.reasoning.trim().is_empty()
            && self.tool_calls.is_empty()
            && self.usage.is_empty()
            && self.outcome.is_none()
            && self.error.is_none()
    }
}

/// Shared `is_false` predicate for `skip_serializing_if`.
pub(crate) fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_usage_merges_and_totals_cost() {
        let mut total = AgentUsage {
            input_tokens: 10,
            cost_usd: Some(0.5),
            ..Default::default()
        };
        total.merge(&AgentUsage {
            input_tokens: 5,
            output_tokens: 2,
            cost_usd: Some(0.25),
            ..Default::default()
        });
        assert_eq!(total.input_tokens, 15);
        assert_eq!(total.output_tokens, 2);
        assert_eq!(total.cost_usd, Some(0.75));
        assert_eq!(total.reported_cost(), Some(AgentCost::usd(0.75)));

        assert!(AgentUsage::default().is_empty());
        assert!(!total.is_empty());
    }

    #[test]
    fn agent_usage_replaces_cumulative_snapshots() {
        let mut usage = AgentUsage::default();
        usage.replace(&AgentUsage {
            context_used: Some(1_000),
            context_size: Some(200_000),
            cost: Some(AgentCost::usd(0.01)),
            ..Default::default()
        });
        usage.replace(&AgentUsage {
            context_used: Some(2_000),
            context_size: Some(200_000),
            cost: Some(AgentCost::usd(0.02)),
            ..Default::default()
        });
        assert_eq!(usage.reported_cost(), Some(AgentCost::usd(0.02)));
        assert_eq!(usage.context_used, Some(2_000));
    }

    #[test]
    fn agent_cost_preserves_non_usd_units_and_merges_same_units() {
        let cost = AgentCost::new(0.5, "credits");
        assert!(!cost.is_usd());
        assert_eq!(cost.display(), "0.5000 credits");

        let mut total = cost.clone();
        assert!(total.merge(&AgentCost::new(0.25, "Credits")));
        assert_eq!(total.amount, 0.75);
        assert!(!total.merge(&AgentCost::usd(0.5)));
        assert_eq!(total.amount, 0.75);

        let mut usage = AgentUsage::default();
        usage.set_cost(Some(AgentCost::new(0.5, "credits")));
        assert_eq!(usage.cost_usd, None);
        assert_eq!(usage.reported_cost(), Some(AgentCost::new(0.5, "credits")));

        usage.set_cost(Some(AgentCost::usd(0.25)));
        assert_eq!(usage.cost_usd, Some(0.25));
        assert_eq!(usage.cost, Some(AgentCost::usd(0.25)));
    }

    #[test]
    fn legacy_cost_usd_payload_decodes_as_a_usd_cost() {
        let usage: AgentUsage =
            serde_json::from_value(serde_json::json!({ "input_tokens": 4, "cost_usd": 0.125 }))
                .unwrap();
        assert_eq!(usage.cost, None);
        assert_eq!(usage.cost_usd, Some(0.125));
        assert_eq!(usage.reported_cost(), Some(AgentCost::usd(0.125)));
    }

    #[test]
    fn non_usd_cost_round_trips_without_polluting_cost_usd() {
        let usage = AgentUsage {
            cost: Some(AgentCost::new(0.75, "credits")),
            ..Default::default()
        };
        let json = serde_json::to_value(&usage).unwrap();
        assert_eq!(json["cost"]["amount"], 0.75);
        assert_eq!(json["cost"]["currency"], "credits");
        assert!(json.get("cost_usd").is_none());

        let back: AgentUsage = serde_json::from_value(json).unwrap();
        assert_eq!(back.reported_cost(), Some(AgentCost::new(0.75, "credits")));
        assert_eq!(back.cost_usd, None);
    }

    #[test]
    fn run_summary_round_trips_and_defaults() {
        let summary = RunSummary {
            session_id: Some("ses_1".to_string()),
            title: Some("Fix it".to_string()),
            text: "done".to_string(),
            reasoning: "hmm".to_string(),
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                name: "bash".to_string(),
                input: serde_json::json!({ "command": "ls" }),
                ok: true,
                output: Some(serde_json::json!("a\nb\n")),
                detail: None,
            }],
            usage: AgentUsage {
                input_tokens: 3,
                cost_usd: Some(0.01),
                ..Default::default()
            },
            outcome: Some(IdleOutcome::Succeeded),
            error: None,
        };
        let json = serde_json::to_value(&summary).unwrap();
        assert_eq!(json["outcome"], "succeeded");
        assert_eq!(json["tool_calls"][0]["name"], "bash");
        let back: RunSummary = serde_json::from_value(json).unwrap();
        assert_eq!(back, summary);

        // A minimal legacy payload decodes with all defaults.
        let minimal: RunSummary = serde_json::from_str("{}").unwrap();
        assert_eq!(minimal, RunSummary::default());
        assert!(minimal.outcome.is_none());
    }

    #[test]
    fn run_summary_is_empty_ignores_identity_fields() {
        // A default summary and a title/session-only summary carry no log.
        assert!(RunSummary::default().is_empty());
        assert!(RunSummary {
            session_id: Some("ses_1".to_string()),
            title: Some("Fix it".to_string()),
            ..Default::default()
        }
        .is_empty());
        // Any renderable field makes it non-empty.
        assert!(!RunSummary {
            text: "done".to_string(),
            ..Default::default()
        }
        .is_empty());
        assert!(!RunSummary {
            tool_calls: vec![ToolCall::default()],
            ..Default::default()
        }
        .is_empty());
        assert!(!RunSummary {
            outcome: Some(IdleOutcome::Failed),
            ..Default::default()
        }
        .is_empty());
        assert!(!RunSummary {
            error: Some("boom".to_string()),
            ..Default::default()
        }
        .is_empty());
        assert!(!RunSummary {
            usage: AgentUsage {
                input_tokens: 1,
                ..Default::default()
            },
            ..Default::default()
        }
        .is_empty());
    }

    #[test]
    fn plan_and_commands_replace_whole_lists_and_default_unknowns() {
        let empty_plan = AgentStateEvent::Plan {
            entries: Vec::new(),
        };
        let json = serde_json::to_value(&empty_plan).unwrap();
        assert_eq!(json, serde_json::json!({ "event": "plan", "entries": [] }));
        assert_eq!(
            serde_json::from_value::<AgentStateEvent>(json).unwrap(),
            empty_plan
        );

        let empty_commands = AgentStateEvent::AvailableCommands {
            commands: Vec::new(),
        };
        let json = serde_json::to_value(&empty_commands).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "event": "available_commands", "commands": [] })
        );
        assert_eq!(
            serde_json::from_value::<AgentStateEvent>(json).unwrap(),
            empty_commands
        );

        // Unknown priorities/statuses degrade instead of failing the whole event.
        let decoded: AgentStateEvent = serde_json::from_value(serde_json::json!({
            "event": "plan",
            "entries": [{ "content": "future", "priority": "urgent", "status": "blocked" }]
        }))
        .unwrap();
        assert_eq!(
            decoded,
            AgentStateEvent::Plan {
                entries: vec![AgentPlanEntry {
                    content: "future".to_string(),
                    priority: AgentPlanPriority::Unknown,
                    status: AgentPlanStatus::Unknown,
                }],
            }
        );

        // Fields missing from an older/newer payload use their defaults.
        let lean: AgentStateEvent = serde_json::from_value(serde_json::json!({
            "event": "plan",
            "entries": [{ "content": "step" }]
        }))
        .unwrap();
        assert_eq!(
            lean,
            AgentStateEvent::Plan {
                entries: vec![AgentPlanEntry {
                    content: "step".to_string(),
                    priority: AgentPlanPriority::Medium,
                    status: AgentPlanStatus::Pending,
                }],
            }
        );
    }

    #[test]
    fn agent_state_event_round_trips() {
        let events = [
            AgentStateEvent::Session {
                session_id: Some("ses_1".to_string()),
                title: None,
                model: Some("model".to_string()),
            },
            AgentStateEvent::TurnStarted,
            AgentStateEvent::TextDelta {
                role: MessageRole::Assistant,
                text: "hi".to_string(),
            },
            AgentStateEvent::ReasoningDelta {
                text: "why".to_string(),
            },
            AgentStateEvent::ContentBlock {
                role: ContentRole::Assistant,
                content: MessageContent::Image {
                    mime_type: "image/png".to_string(),
                    uri: Some("file:///tmp/a.png".to_string()),
                },
            },
            AgentStateEvent::ToolStarted {
                id: "c1".to_string(),
                name: "read".to_string(),
                input: serde_json::json!({}),
                detail: None,
            },
            AgentStateEvent::ToolFinished {
                id: "c1".to_string(),
                name: "read".to_string(),
                ok: false,
                output: None,
                detail: None,
            },
            AgentStateEvent::Usage {
                usage: AgentUsage::default(),
                snapshot: true,
            },
            AgentStateEvent::Plan {
                entries: vec![AgentPlanEntry {
                    content: "Read the issue".to_string(),
                    priority: AgentPlanPriority::High,
                    status: AgentPlanStatus::InProgress,
                }],
            },
            AgentStateEvent::AvailableCommands {
                commands: vec![AgentCommand {
                    name: "web".to_string(),
                    description: "Search the web".to_string(),
                    input_hint: Some("query".to_string()),
                }],
            },
            AgentStateEvent::Idle {
                outcome: IdleOutcome::Interrupted,
            },
            AgentStateEvent::Error {
                message: "boom".to_string(),
            },
        ];
        for event in events {
            let json = serde_json::to_value(&event).unwrap();
            let back: AgentStateEvent = serde_json::from_value(json).unwrap();
            assert_eq!(back, event);
        }
    }

    #[test]
    fn usage_event_without_snapshot_decodes_as_an_incremental_step() {
        let event: AgentStateEvent = serde_json::from_value(serde_json::json!({
            "event": "usage",
            "usage": { "input_tokens": 2, "cost_usd": 0.01 }
        }))
        .unwrap();
        assert_eq!(
            event,
            AgentStateEvent::Usage {
                usage: AgentUsage {
                    input_tokens: 2,
                    cost_usd: Some(0.01),
                    ..Default::default()
                },
                snapshot: false,
            }
        );

        let snapshot: AgentStateEvent = serde_json::from_value(serde_json::json!({
            "event": "usage",
            "snapshot": true,
            "usage": { "cost": { "amount": 0.02, "currency": "USD" } }
        }))
        .unwrap();
        assert!(matches!(
            snapshot,
            AgentStateEvent::Usage { snapshot: true, .. }
        ));
    }

    #[test]
    fn message_content_is_metadata_only_and_has_a_safe_placeholder() {
        let content = [
            MessageContent::Image {
                mime_type: "image/png".to_string(),
                uri: Some("https://example.com/a.png".to_string()),
            },
            MessageContent::Audio {
                mime_type: "audio/wav".to_string(),
            },
            MessageContent::ResourceLink {
                uri: "file:///tmp/a.txt".to_string(),
                name: "a.txt".to_string(),
                mime_type: Some("text/plain".to_string()),
                title: Some("A".to_string()),
                description: Some("notes".to_string()),
                size: Some(12),
            },
            MessageContent::Resource {
                uri: "file:///tmp/b.bin".to_string(),
                mime_type: Some("application/octet-stream".to_string()),
                binary: true,
            },
            MessageContent::Unknown,
        ];

        for block in content {
            let json = serde_json::to_value(&block).unwrap();
            assert!(json.get("data").is_none());
            assert!(json.get("blob").is_none());
            assert!(json.get("text").is_none());
            let back: MessageContent = serde_json::from_value(json).unwrap();
            assert_eq!(back, block);
        }

        // A future block tag decodes to the generic variant instead of failing.
        let unknown: MessageContent =
            serde_json::from_value(serde_json::json!({ "type": "future_block" })).unwrap();
        assert_eq!(unknown, MessageContent::Unknown);
        assert_eq!(unknown.placeholder(), "unsupported content block");
    }

    #[test]
    fn content_block_placeholders_are_deterministic_and_non_rendering() {
        let image = MessageContent::Image {
            mime_type: "image/png".to_string(),
            uri: Some("https://example.com/a.png".to_string()),
        };
        assert_eq!(
            image.placeholder(),
            "image image/png <https://example.com/a.png>"
        );
        assert_eq!(
            MessageContent::Audio {
                mime_type: "audio/wav".to_string(),
            }
            .placeholder(),
            "audio audio/wav"
        );
        assert_eq!(
            MessageContent::ResourceLink {
                uri: "file:///tmp/a.txt".to_string(),
                name: "a.txt".to_string(),
                mime_type: Some("text/plain".to_string()),
                title: Some("A".to_string()),
                description: None,
                size: Some(12),
            }
            .placeholder(),
            "link a.txt <file:///tmp/a.txt> text/plain 12 bytes"
        );
        assert_eq!(
            MessageContent::Resource {
                uri: "file:///tmp/b.bin".to_string(),
                mime_type: None,
                binary: true,
            }
            .placeholder(),
            "resource <file:///tmp/b.bin> (binary)"
        );
        assert_eq!(ContentRole::Thought.as_str(), "thought");
    }

    #[test]
    fn tool_call_detail_round_trips_and_legacy_payloads_default() {
        let detail = ToolCallDetail {
            name: Some("bash".to_string()),
            title: Some("Run tests".to_string()),
            kind: Some(ToolKind::Execute),
            status: Some(ToolCallStatus::Completed),
            input: Some(serde_json::json!({ "command": "cargo test" })),
            output: Some(serde_json::json!({ "exit": 0 })),
            content: vec![
                ToolContent::Text {
                    text: "ok".to_string(),
                },
                ToolContent::Diff {
                    path: "src/lib.rs".to_string(),
                    old_text: Some("a".to_string()),
                    new_text: "b".to_string(),
                },
                ToolContent::Terminal {
                    terminal_id: "term_1".to_string(),
                },
                ToolContent::ResourceLink {
                    uri: "file:///tmp/a".to_string(),
                    name: "a".to_string(),
                    mime_type: Some("text/plain".to_string()),
                    title: None,
                    description: None,
                    size: Some(3),
                },
                ToolContent::Image {
                    mime_type: "image/png".to_string(),
                    uri: None,
                },
            ],
            locations: vec![ToolLocation {
                path: "src/lib.rs".to_string(),
                line: Some(12),
            }],
        };
        let json = serde_json::to_value(&detail).unwrap();
        assert_eq!(json["kind"], "execute");
        assert_eq!(json["status"], "completed");
        assert_eq!(json["content"][1]["type"], "diff");
        assert_eq!(json["content"][4]["type"], "image");
        assert!(json["content"][4].get("data").is_none());
        let back: ToolCallDetail = serde_json::from_value(json).unwrap();
        assert_eq!(back, detail);

        // A payload from an older daemon has no `detail` field at all.
        let legacy: AgentStateEvent = serde_json::from_value(serde_json::json!({
            "event": "tool_started",
            "id": "c1",
            "name": "read",
            "input": { "path": "a" }
        }))
        .unwrap();
        assert_eq!(
            legacy,
            AgentStateEvent::ToolStarted {
                id: "c1".to_string(),
                name: "read".to_string(),
                input: serde_json::json!({ "path": "a" }),
                detail: None,
            }
        );

        let unknown_kind: ToolKind = serde_json::from_str("\"future_kind\"").unwrap();
        assert_eq!(unknown_kind, ToolKind::Other);
        let unknown_status: ToolCallStatus = serde_json::from_str("\"future_status\"").unwrap();
        assert_eq!(unknown_status, ToolCallStatus::Unknown);
        assert!(!unknown_status.is_terminal());
        assert!(ToolCallStatus::Completed.is_terminal());
    }

    #[test]
    fn tool_detail_serializes_sparse_without_null_noise() {
        let detail = ToolCallDetail {
            title: Some("Read".to_string()),
            status: Some(ToolCallStatus::Pending),
            ..Default::default()
        };
        let json = serde_json::to_value(&detail).unwrap();
        assert_eq!(json["title"], "Read");
        assert!(json.get("name").is_none());
        assert!(json.get("kind").is_none());
        assert!(json.get("content").is_none());
        assert!(json.get("locations").is_none());
    }

    #[test]
    fn input_request_and_reply_round_trip() {
        let request = InputRequest {
            id: "perm_1".to_string(),
            kind: AwaitingInputKind::Permission,
            message: "Allow?".to_string(),
            options: vec!["Allow once".to_string(), "Reject".to_string()],
            allow_always: true,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["allow_always"], true);
        assert_eq!(json["options"][1], "Reject");
        let back: InputRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back, request);

        // Empty options and `allow_always` are omitted, not sent as null/false.
        let lean = serde_json::to_value(InputRequest {
            options: Vec::new(),
            allow_always: false,
            ..request
        })
        .unwrap();
        assert!(lean.get("options").is_none());
        assert!(lean.get("allow_always").is_none());

        for reply in [
            InputReply::Once,
            InputReply::Always,
            InputReply::Reject,
            InputReply::Value {
                value: "abc".to_string(),
            },
            InputReply::Confirmed { confirmed: true },
            InputReply::Cancelled,
        ] {
            let json = serde_json::to_value(&reply).unwrap();
            let back: InputReply = serde_json::from_value(json).unwrap();
            assert_eq!(back, reply);
        }
    }

    #[test]
    fn agent_activity_round_trips() {
        let activities = [
            AgentActivity::Starting,
            AgentActivity::Thinking,
            AgentActivity::Responding,
            AgentActivity::Tool {
                name: "bash".to_string(),
                description: Some("Run tests".to_string()),
            },
            AgentActivity::Waiting {
                request: InputRequest {
                    id: "perm_1".to_string(),
                    kind: AwaitingInputKind::Permission,
                    message: "Allow?".to_string(),
                    options: Vec::new(),
                    allow_always: false,
                },
            },
            AgentActivity::Idle,
            AgentActivity::Exited { code: Some(0) },
        ];
        for activity in activities {
            let json = serde_json::to_value(&activity).unwrap();
            assert!(json.get("kind").is_some());
            let back: AgentActivity = serde_json::from_value(json).unwrap();
            assert_eq!(back, activity);
        }
    }

    #[test]
    fn usage_period_round_trips_and_defaults() {
        assert_eq!(UsagePeriod::default(), UsagePeriod::Day);
        assert_eq!(
            UsagePeriod::ALL.map(|p| p.as_str()),
            ["day", "week", "month", "year"]
        );
        assert_eq!(UsagePeriod::Week.bucket_unit(), "day");
        assert_eq!(UsagePeriod::Year.bucket_unit(), "month");

        for period in UsagePeriod::ALL {
            let json = serde_json::to_value(period).unwrap();
            assert_eq!(json, serde_json::json!(period.as_str()));
            let back: UsagePeriod = serde_json::from_value(json).unwrap();
            assert_eq!(back, period);
            assert_eq!(period.as_str().parse::<UsagePeriod>(), Ok(period));
        }
        assert!("nope".parse::<UsagePeriod>().is_err());
    }

    #[test]
    fn usage_stats_round_trip_and_total_tokens() {
        let start = DateTime::from_timestamp_millis(1_700_000_000_000).unwrap();
        let usage = AgentUsage {
            input_tokens: 10,
            output_tokens: 4,
            reasoning_tokens: 1,
            cache_read_tokens: 7,
            cache_write_tokens: 2,
            cost_usd: Some(0.25),
            cost: None,
            context_used: None,
            context_size: None,
        };
        // Cache counters are excluded from the headline total.
        assert_eq!(usage.total_tokens(), 15);

        let stats = UsageStats {
            period: UsagePeriod::Day,
            totals: UsageTotals {
                usage: usage.clone(),
                runs: 3,
            },
            buckets: vec![UsageBucket {
                start,
                end: start + chrono::Duration::hours(1),
                usage,
                runs: 3,
            }],
        };
        let json = serde_json::to_value(&stats).unwrap();
        assert_eq!(json["period"], "day");
        assert_eq!(json["totals"]["runs"], 3);
        assert_eq!(json["buckets"][0]["usage"]["input_tokens"], 10);
        let back: UsageStats = serde_json::from_value(json).unwrap();
        assert_eq!(back, stats);
        assert_eq!(back.buckets[0].total_tokens(), 15);

        // An absent cost serializes as null and decodes back to `None`.
        let no_cost = UsageBucket {
            usage: AgentUsage::default(),
            runs: 0,
            ..back.buckets[0].clone()
        };
        let json = serde_json::to_value(no_cost).unwrap();
        assert_eq!(json["usage"]["cost_usd"], serde_json::Value::Null);
    }
}
