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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

impl AgentUsage {
    /// Fold `other` into `self` (per-step usage summed into a run total).
    pub fn merge(&mut self, other: &AgentUsage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.reasoning_tokens += other.reasoning_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
        if let Some(cost) = other.cost_usd {
            *self.cost_usd.get_or_insert(0.0) += cost;
        }
    }

    /// Whether nothing has been reported (all counters zero, no cost).
    pub fn is_empty(&self) -> bool {
        self.input_tokens == 0
            && self.output_tokens == 0
            && self.reasoning_tokens == 0
            && self.cache_read_tokens == 0
            && self.cache_write_tokens == 0
            && self.cost_usd.is_none()
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
    /// Token/cost totals for the bucket. `cost_usd` is `None` when no run in the
    /// bucket reported a cost.
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
}

/// How a run or turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdleOutcome {
    Succeeded,
    Failed,
    Interrupted,
}

/// One normalized observation about a session, in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// A tool invocation started.
    ToolStarted {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Partial tool output (streaming transports).
    ToolUpdated {
        id: String,
        partial: serde_json::Value,
    },
    /// A tool invocation finished.
    ToolFinished {
        id: String,
        name: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<serde_json::Value>,
    },
    /// The agent is blocked on the user.
    InputRequested { request: InputRequest },
    /// A previously requested input was answered (or dismissed).
    InputResolved { id: String },
    /// Token/cost usage for a step.
    Usage { usage: AgentUsage },
    /// The session's title changed.
    Title { title: String },
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

        assert!(AgentUsage::default().is_empty());
        assert!(!total.is_empty());
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
            AgentStateEvent::ToolStarted {
                id: "c1".to_string(),
                name: "read".to_string(),
                input: serde_json::json!({}),
            },
            AgentStateEvent::ToolFinished {
                id: "c1".to_string(),
                name: "read".to_string(),
                ok: false,
                output: None,
            },
            AgentStateEvent::Usage {
                usage: AgentUsage::default(),
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
