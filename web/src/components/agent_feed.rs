//! Live, pretty-formatted agent event feed for the Task view.
//!
//! The daemon normalizes every structured agent into `AgentStateEvent`s and
//! pushes them as non-durable `agent.event` notifications (`tasks.events`
//! backfills after a reload). This module turns that raw stream into a
//! human-readable, expandable feed rendered above the post-run `OutputViewer`.
//!
//! The mapping is deliberately pure and host-testable ([`feed_item`] /
//! [`build_feed`]); only the reply controls touch the network.

use std::collections::{HashMap, HashSet};

use dioxus::prelude::*;
use favetto_wire::model::{
    AgentStateEvent, AgentUsage, IdleOutcome, InputRequest, MessageRole, RunSummary,
};
use favetto_wire::rpc::AgentEventPush;

use crate::components::highlight::{highlight, pretty, readable_json, Readable};
use crate::components::icon::{AppIcon, AppIconKind};
use crate::components::markdown::Markdown;
use crate::components::output_viewer::{strip_ansi, truncate_lines};
use crate::components::toast::ToastKind;
use crate::state::AppState;
use crate::views::inbox::{submit_reply, AwaitingCard, ReplyIntent, SCREEN_DETECTED_REPLY};

/// Lines of a reasoning block (or tool result) shown before the expander.
pub const FEED_COLLAPSED_LINES: usize = 20;

/// The number of pixels from the bottom within which the feed keeps following.
pub const FOLLOW_THRESHOLD: f64 = 24.0;

/// Whether a scroll position is close enough to the bottom to keep following.
pub fn at_bottom(scroll_top: f64, client_height: f64, scroll_height: f64) -> bool {
    scroll_top + client_height >= scroll_height - FOLLOW_THRESHOLD
}

/// The Phosphor icon for a tool card header, keyed off the tool's name.
pub fn tool_icon(name: &str) -> AppIconKind {
    let name = name.to_ascii_lowercase();
    if ["bash", "shell", "exec", "command", "run", "terminal"]
        .iter()
        .any(|k| name.contains(k))
    {
        AppIconKind::ToolTerminal
    } else if ["write", "edit", "patch", "create", "append"]
        .iter()
        .any(|k| name.contains(k))
    {
        AppIconKind::ToolEdit
    } else if ["read", "view", "cat", "open"]
        .iter()
        .any(|k| name.contains(k))
    {
        AppIconKind::ToolRead
    } else if ["grep", "search", "find", "glob", "query"]
        .iter()
        .any(|k| name.contains(k))
    {
        AppIconKind::ToolSearch
    } else if ["fetch", "http", "web", "curl", "download"]
        .iter()
        .any(|k| name.contains(k))
    {
        AppIconKind::ToolFetch
    } else if ["list", "ls", "dir", "tree"]
        .iter()
        .any(|k| name.contains(k))
    {
        AppIconKind::ToolList
    } else if ["task", "agent", "todo", "plan"]
        .iter()
        .any(|k| name.contains(k))
    {
        AppIconKind::ToolTodo
    } else {
        AppIconKind::ToolGeneric
    }
}

/// The full and collapsed forms of a readable block, plus whether it was cut.
fn block_forms(readable: &Readable) -> (String, String, bool) {
    let full = match readable {
        Readable::Text(text) => text.clone(),
        Readable::Json(value) => pretty(value),
    };
    let (preview, cut) = truncate_lines(&full, FEED_COLLAPSED_LINES);
    (full, preview, cut)
}

/// Render a code block: coloured spans for JSON, plain text otherwise.
fn readable_pre(class: &str, readable: &Readable, shown: &str) -> Element {
    match readable {
        Readable::Json(_) => {
            let tokens = highlight(shown);
            let class = format!("{class} feed-code");
            rsx! {
                pre { class: "{class}",
                    for (index, token) in tokens.into_iter().enumerate() {
                        span {
                            key: "{index}",
                            class: token.role.class(),
                            "{token.text}"
                        }
                    }
                }
            }
        }
        Readable::Text(_) => rsx! {
            pre { class: "{class}", "{shown}" }
        },
    }
}

/// The status of a tool card in the feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    /// `ToolStarted` (or `ToolUpdated`) but no `ToolFinished` yet.
    Running,
    /// `ToolFinished { ok: true }`.
    Ok,
    /// `ToolFinished { ok: false }`.
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
}

/// One renderable entry in the agent feed.
#[derive(Debug, Clone, PartialEq)]
pub enum FeedItem {
    /// A text block from the assistant/user/system.
    Message { role: MessageRole, text: String },
    /// Chain-of-thought text (dimmed, collapsible).
    Reasoning { text: String },
    /// A tool invocation, from `Started` to `Finished`.
    Tool {
        id: String,
        name: String,
        input: serde_json::Value,
        status: ToolStatus,
        output: Option<serde_json::Value>,
    },
    /// Token/cost usage for a step.
    Usage(AgentUsage),
    /// A session-level error.
    Error { message: String },
    /// A turn/run boundary (`Idle`).
    Boundary(IdleOutcome),
    /// A prompt the agent is blocked on.
    InputRequest(InputRequest),
    /// The agent reported (or changed) its session identity/title.
    Session { title: Option<String> },
    /// The session's title changed.
    Title(String),
}

/// Map one normalized event to a feed item.
///
/// Returns `None` for session bookkeeping that the feed does not render:
/// `TurnStarted` (implicit separator) and `InputResolved` (clears the inline
/// prompt, see [`build_feed`]).
pub fn feed_item(event: &AgentStateEvent) -> Option<FeedItem> {
    match event {
        AgentStateEvent::Session { title, .. } => Some(FeedItem::Session {
            title: title.clone(),
        }),
        AgentStateEvent::TurnStarted => None,
        AgentStateEvent::TextDelta { role, text } => Some(FeedItem::Message {
            role: *role,
            text: text.clone(),
        }),
        AgentStateEvent::ReasoningDelta { text } => {
            Some(FeedItem::Reasoning { text: text.clone() })
        }
        AgentStateEvent::ToolStarted { id, name, input } => Some(FeedItem::Tool {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
            status: ToolStatus::Running,
            output: None,
        }),
        AgentStateEvent::ToolUpdated { id, partial } => Some(FeedItem::Tool {
            id: id.clone(),
            name: String::new(),
            input: serde_json::Value::Null,
            status: ToolStatus::Running,
            output: Some(partial.clone()),
        }),
        AgentStateEvent::ToolFinished {
            id,
            name,
            ok,
            output,
        } => Some(FeedItem::Tool {
            id: id.clone(),
            name: name.clone(),
            input: serde_json::Value::Null,
            status: if *ok {
                ToolStatus::Ok
            } else {
                ToolStatus::Error
            },
            output: output.clone(),
        }),
        AgentStateEvent::InputRequested { request } => {
            Some(FeedItem::InputRequest(request.clone()))
        }
        AgentStateEvent::InputResolved { .. } => None,
        AgentStateEvent::Usage { usage } => Some(FeedItem::Usage(usage.clone())),
        AgentStateEvent::Title { title } => Some(FeedItem::Title(title.clone())),
        AgentStateEvent::Idle { outcome } => Some(FeedItem::Boundary(*outcome)),
        AgentStateEvent::Error { message } => Some(FeedItem::Error {
            message: message.clone(),
        }),
    }
}

/// Fold an ordered event window into display items.
///
/// Tool events reference their call by id, so they merge into the original
/// `ToolStarted` card; a resolved/dismissed input prompt is dropped once an
/// `InputResolved` (or a turn `Idle`) follows it.
pub fn build_feed(events: &[AgentEventPush]) -> Vec<FeedItem> {
    let mut items: Vec<FeedItem> = Vec::new();
    let mut tools: HashMap<String, usize> = HashMap::new();
    let mut pending_input: Option<(usize, String)> = None;
    let mut resolved: HashSet<usize> = HashSet::new();

    for push in events {
        match &push.event {
            AgentStateEvent::ToolStarted { id, name, input } => {
                tools.insert(id.clone(), items.len());
                items.push(FeedItem::Tool {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    status: ToolStatus::Running,
                    output: None,
                });
            }
            AgentStateEvent::ToolUpdated { id, partial } => {
                if let Some(&index) = tools.get(id) {
                    if let FeedItem::Tool { output, .. } = &mut items[index] {
                        *output = Some(partial.clone());
                    }
                } else {
                    tools.insert(id.clone(), items.len());
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
            } => {
                let status = if *ok {
                    ToolStatus::Ok
                } else {
                    ToolStatus::Error
                };
                if let Some(&index) = tools.get(id) {
                    if let FeedItem::Tool {
                        status: slot,
                        output: slot_output,
                        ..
                    } = &mut items[index]
                    {
                        *slot = status;
                        if output.is_some() {
                            *slot_output = output.clone();
                        }
                    }
                } else {
                    tools.insert(id.clone(), items.len());
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
                let index = items.len();
                pending_input = Some((index, request.id.clone()));
                items.push(FeedItem::InputRequest(request.clone()));
            }
            AgentStateEvent::InputResolved { id } => {
                if let Some((index, pending)) = &pending_input {
                    if pending == id {
                        resolved.insert(*index);
                        pending_input = None;
                    }
                }
            }
            AgentStateEvent::Idle { outcome } => {
                if let Some((index, _)) = pending_input.take() {
                    resolved.insert(index);
                }
                items.push(FeedItem::Boundary(*outcome));
            }
            other => {
                if let Some(item) = feed_item(other) {
                    items.push(item);
                }
            }
        }
    }

    items
        .into_iter()
        .enumerate()
        .filter(|(index, _)| !resolved.contains(index))
        .map(|(_, item)| item)
        .collect()
}

/// Map a persisted [`RunSummary`] onto the feed view-model, so a finished run's
/// output renders as a human-readable log after a reload (or daemon restart)
/// when the ephemeral live feed is gone.
///
/// The summary folds events together, so the finer ordering is lost; this emits
/// a canonical order — session, message, reasoning, tool cards, usage, error,
/// boundary — matching how a coding-agent TUI reads top to bottom.
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
        items.push(FeedItem::Error {
            message: error.clone(),
        });
    }
    if let Some(outcome) = summary.outcome {
        items.push(FeedItem::Boundary(outcome));
    }
    items
}

/// The live feed section rendered above the task's output viewer.
#[component]
pub fn AgentFeed(
    events: Vec<AgentEventPush>,
    busy: Signal<bool>,
    toast: Signal<Option<(ToastKind, String)>>,
    refresh: Signal<u64>,
) -> Element {
    let items = build_feed(&events);
    if items.is_empty() {
        return rsx! {};
    }
    let session_id = events
        .last()
        .map(|event| event.session_id.clone())
        .unwrap_or_default();
    // The newest event's sequence bumps on every push (even a merged
    // `ToolUpdated`), so the feed re-anchors and follows streaming output.
    let revision = events.last().map(|event| event.seq).unwrap_or(0);

    rsx! {
        section { class: "agent-feed-section",
            h2 { "Live feed" }
            FeedList { items, session_id, revision, busy, toast, refresh }
        }
    }
}

/// A persisted run rendered as a human-readable agent log.
///
/// Fed by [`summary_feed`], this is the post-run counterpart to [`AgentFeed`]:
/// it renders the same rows from the durable `task.output` summary, so the log
/// survives a reload or daemon restart. A persisted log has no live session to
/// reply to, so [`FeedItem::InputRequest`] cannot occur.
#[component]
pub fn AgentLog(items: Vec<FeedItem>) -> Element {
    if items.is_empty() {
        return rsx! {};
    }
    let busy = use_signal(|| false);
    let toast = use_signal(|| None::<(ToastKind, String)>);
    let refresh = use_signal(|| 0u64);
    let revision = items.len() as u64;
    rsx! {
        FeedList { items, session_id: String::new(), revision, busy, toast, refresh }
    }
}

/// The scrollable list of feed rows shared by the live feed and the persisted
/// post-run log.
///
/// The list auto-follows: while the user is at (or near) the bottom it stays
/// pinned to the newest row; scrolling up pauses it and reveals a jump control
/// that resumes following.
#[component]
fn FeedList(
    items: Vec<FeedItem>,
    session_id: String,
    revision: u64,
    busy: Signal<bool>,
    toast: Signal<Option<(ToastKind, String)>>,
    refresh: Signal<u64>,
) -> Element {
    let mut follow = use_signal(|| true);
    let mut element = use_signal(|| None::<web_sys::Element>);
    let scroll_to_bottom = move || {
        if let Some(el) = element() {
            el.set_scroll_top(el.scroll_height());
        }
    };

    rsx! {
        div { class: "agent-feed-wrap",
            div {
                class: "agent-feed",
                onmounted: move |evt| {
                    element.set(evt.data().downcast::<web_sys::Element>().cloned());
                    if follow() {
                        scroll_to_bottom();
                    }
                },
                onscroll: move |evt| {
                    let data = evt.data();
                    follow.set(at_bottom(
                        data.scroll_top(),
                        f64::from(data.client_height()),
                        f64::from(data.scroll_height()),
                    ));
                },
                for (index, item) in items.into_iter().enumerate() {
                    FeedRow {
                        key: "{index}",
                        item,
                        session_id: session_id.clone(),
                        busy,
                        toast,
                        refresh,
                    }
                }
                // Re-keyed whenever an event arrives, so the mount hook fires
                // exactly when the feed should catch back up to the bottom.
                div {
                    key: "anchor-{revision}",
                    class: "feed-anchor",
                    aria_hidden: "true",
                    onmounted: move |_| {
                        if follow() {
                            scroll_to_bottom();
                        }
                    },
                }
            }
            if !follow() {
                button {
                    class: "feed-jump",
                    onclick: move |_| {
                        follow.set(true);
                        scroll_to_bottom();
                    },
                    "Jump to latest"
                    AppIcon { kind: AppIconKind::ArrowDown, class: "feed-jump-arrow" }
                }
            }
        }
    }
}

/// One feed entry. Shares a single expander signal across the row's variants.
#[component]
fn FeedRow(
    item: FeedItem,
    session_id: String,
    busy: Signal<bool>,
    toast: Signal<Option<(ToastKind, String)>>,
    refresh: Signal<u64>,
) -> Element {
    let mut expanded = use_signal(|| false);
    match item {
        FeedItem::Message { role, text } => {
            let label = match role {
                MessageRole::System => "system",
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::Tool => "tool",
            };
            let role_class = match role {
                MessageRole::System => "role-system",
                MessageRole::User => "role-user",
                MessageRole::Assistant => "role-assistant",
                MessageRole::Tool => "role-tool",
            };
            let clean = strip_ansi(&text);
            let (preview, cut) = truncate_lines(&clean, FEED_COLLAPSED_LINES);
            let shown = if expanded() { clean.clone() } else { preview };
            let is_assistant = role == MessageRole::Assistant;
            let item_class = format!("feed-item feed-message {role_class}");
            let role_label = format!("feed-role {role_class}");
            // Clamp by height only while collapsed over the line threshold, so
            // a short-but-tall block is never clipped without an expander.
            let markdown_class = if expanded() || !cut {
                "feed-markdown"
            } else {
                "feed-markdown clamped"
            };
            rsx! {
                div { class: "{item_class}",
                    span { class: "{role_label}", "{label}" }
                    if is_assistant {
                        // Assistant prose is Markdown; a long block clamps by
                        // height so fenced code and lists stay well-formed.
                        div { class: "{markdown_class}",
                            Markdown { source: clean.clone() }
                        }
                    } else {
                        pre { class: "feed-text", "{shown}" }
                    }
                    if cut {
                        button {
                            class: "feed-toggle",
                            onclick: move |_| expanded.set(!expanded()),
                            if expanded() { "Show less" } else { "Show more" }
                        }
                    }
                }
            }
        }
        FeedItem::Reasoning { text } => {
            let clean = strip_ansi(&text);
            let (preview, cut) = truncate_lines(&clean, FEED_COLLAPSED_LINES);
            let shown = if expanded() { clean.clone() } else { preview };
            rsx! {
                div { class: "feed-item feed-reasoning",
                    span { class: "feed-role role-reasoning", "thinking" }
                    pre { class: "feed-text", "{shown}" }
                    if cut {
                        button {
                            class: "feed-toggle",
                            onclick: move |_| expanded.set(!expanded()),
                            if expanded() { "Show less" } else { "Show more" }
                        }
                    }
                }
            }
        }
        FeedItem::Tool {
            name,
            input,
            status,
            output,
            ..
        } => {
            // Tool input/result render readably: recognizable envelopes are
            // unwrapped to text, structured values are pretty-printed and
            // syntax-highlighted, and both collapse to a preview.
            let input = readable_json(&input);
            let output = output.as_ref().map(readable_json);
            let input_forms = (!input.is_empty()).then(|| block_forms(&input));
            let output_forms = output
                .as_ref()
                .filter(|readable| !readable.is_empty())
                .map(block_forms);
            let cut = input_forms.as_ref().is_some_and(|(_, _, cut)| *cut)
                || output_forms.as_ref().is_some_and(|(_, _, cut)| *cut);
            let is_expanded = expanded();
            let input_shown = input_forms.as_ref().map(|(full, preview, _)| {
                if is_expanded {
                    full.clone()
                } else {
                    preview.clone()
                }
            });
            let output_shown = output_forms.as_ref().map(|(full, preview, _)| {
                if is_expanded {
                    full.clone()
                } else {
                    preview.clone()
                }
            });
            let label = if name.is_empty() {
                "tool"
            } else {
                name.as_str()
            };
            let icon = tool_icon(label);
            let card = format!("feed-item feed-tool tool-{}", status.label());
            let chip = format!("tool-chip tool-{}", status.label());
            rsx! {
                div { class: "{card}",
                    div { class: "feed-tool-head",
                        AppIcon { kind: icon, class: "feed-tool-icon" }
                        span { class: "feed-tool-name", "{label}" }
                        span { class: "{chip}", "{status.label()}" }
                    }
                    if let Some(shown) = &input_shown {
                        if !shown.trim().is_empty() {
                            {readable_pre("feed-tool-input", &input, shown)}
                        }
                    }
                    if let Some(readable) = &output {
                        if let Some(shown) = &output_shown {
                            if !shown.trim().is_empty() {
                                {readable_pre("feed-tool-output", readable, shown)}
                            }
                        }
                    }
                    if cut {
                        button {
                            class: "feed-toggle",
                            onclick: move |_| expanded.set(!expanded()),
                            if expanded() { "Show less" } else { "Show more" }
                        }
                    }
                }
            }
        }
        FeedItem::Usage(usage) => {
            let cost = usage
                .cost_usd
                .map(|cost| format!(" · ${cost:.4}"))
                .unwrap_or_default();
            rsx! {
                div { class: "feed-item feed-usage",
                    "{usage.total_tokens()} tokens ({usage.input_tokens} in / {usage.output_tokens} out){cost}"
                }
            }
        }
        FeedItem::Error { message } => rsx! {
            div { class: "feed-item feed-error", role: "alert", "{message}" }
        },
        FeedItem::Boundary(outcome) => {
            let label = match outcome {
                IdleOutcome::Succeeded => "succeeded",
                IdleOutcome::Failed => "failed",
                IdleOutcome::Interrupted => "interrupted",
            };
            rsx! {
                div { class: "feed-item feed-boundary", "— {label} —" }
            }
        }
        FeedItem::InputRequest(request) => {
            let card = AwaitingCard {
                session_id: session_id.clone(),
                request_id: Some(request.id.clone()),
                kind: request.kind,
                message: request.message.clone(),
                options: request.options.clone(),
                allow_always: request.allow_always,
            };
            rsx! { InlinePrompt { card, busy, toast, refresh } }
        }
        FeedItem::Session { title } => {
            let label = title.unwrap_or_else(|| "session started".to_string());
            rsx! {
                div { class: "feed-item feed-session", "{label}" }
            }
        }
        FeedItem::Title(title) => rsx! {
            div { class: "feed-item feed-title", "{title}" }
        },
    }
}

/// An inline awaiting-input prompt built straight from the `agent.event`
/// stream, so a blocked run can be answered from the Task view without a
/// round trip to `agents.list`.
#[component]
fn InlinePrompt(
    card: AwaitingCard,
    busy: Signal<bool>,
    toast: Signal<Option<(ToastKind, String)>>,
    refresh: Signal<u64>,
) -> Element {
    let state = use_context::<AppState>();
    let mut draft = use_signal(String::new);
    let allow_always = card.allow_always;
    let options = card.options.clone();
    let submit_card = card.clone();
    let submit = use_callback(move |intent: ReplyIntent| {
        let Some(request_id) = submit_card.request_id.clone() else {
            let mut toast = toast;
            toast.set(Some((ToastKind::Error, SCREEN_DETECTED_REPLY.to_string())));
            return;
        };
        let reply = submit_card.to_reply(&intent);
        submit_reply(
            state,
            busy,
            toast,
            refresh,
            submit_card.session_id.clone(),
            request_id,
            reply,
        );
    });

    rsx! {
        div { class: "feed-item feed-prompt awaiting-card",
            p { class: "awaiting-prompt", "{card.message}" }
            if options.is_empty() {
                div { class: "awaiting-free-text",
                    input {
                        class: "awaiting-input",
                        r#type: "text",
                        placeholder: "Type a reply…",
                        aria_label: "Reply to the agent prompt",
                        value: "{draft}",
                        disabled: busy(),
                        oninput: move |event| draft.set(event.value()),
                    }
                    button {
                        class: "btn primary option-button",
                        disabled: busy(),
                        onclick: move |_| submit.call(ReplyIntent::Text(draft())),
                        "Send"
                    }
                }
            } else {
                div { class: "awaiting-options",
                    for (index, option) in options.iter().enumerate() {
                        button {
                            class: "btn option-button",
                            disabled: busy(),
                            onclick: move |_| submit.call(ReplyIntent::Option(index)),
                            "{option}"
                        }
                    }
                    if allow_always {
                        button {
                            class: "btn option-button",
                            disabled: busy(),
                            onclick: move |_| submit.call(ReplyIntent::Always),
                            "Always"
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use favetto_wire::model::{AgentUsage, AwaitingInputKind, InputRequest, ToolCall};

    fn push(seq: u64, event: AgentStateEvent) -> AgentEventPush {
        AgentEventPush {
            session_id: "ses_1".to_string(),
            task_id: Some("task".to_string()),
            seq,
            event,
        }
    }

    #[test]
    fn feed_item_maps_every_variant_or_drops_it_deliberately() {
        let request = InputRequest {
            id: "req_1".to_string(),
            kind: AwaitingInputKind::Permission,
            message: "Allow?".to_string(),
            options: vec!["once".to_string()],
            allow_always: true,
        };
        assert_eq!(
            feed_item(&AgentStateEvent::TextDelta {
                role: MessageRole::Assistant,
                text: "hi".to_string(),
            }),
            Some(FeedItem::Message {
                role: MessageRole::Assistant,
                text: "hi".to_string(),
            })
        );
        assert!(matches!(
            feed_item(&AgentStateEvent::ReasoningDelta {
                text: "why".to_string()
            }),
            Some(FeedItem::Reasoning { .. })
        ));
        assert!(matches!(
            feed_item(&AgentStateEvent::ToolStarted {
                id: "c1".to_string(),
                name: "read".to_string(),
                input: serde_json::json!({}),
            }),
            Some(FeedItem::Tool {
                status: ToolStatus::Running,
                ..
            })
        ));
        assert!(matches!(
            feed_item(&AgentStateEvent::ToolUpdated {
                id: "c1".to_string(),
                partial: serde_json::json!("x"),
            }),
            Some(FeedItem::Tool {
                status: ToolStatus::Running,
                ..
            })
        ));
        assert!(matches!(
            feed_item(&AgentStateEvent::ToolFinished {
                id: "c1".to_string(),
                name: "read".to_string(),
                ok: false,
                output: None,
            }),
            Some(FeedItem::Tool {
                status: ToolStatus::Error,
                ..
            })
        ));
        assert_eq!(
            feed_item(&AgentStateEvent::InputRequested {
                request: request.clone()
            }),
            Some(FeedItem::InputRequest(request))
        );
        assert!(matches!(
            feed_item(&AgentStateEvent::Usage {
                usage: AgentUsage::default()
            }),
            Some(FeedItem::Usage(_))
        ));
        assert_eq!(
            feed_item(&AgentStateEvent::Title {
                title: "t".to_string()
            }),
            Some(FeedItem::Title("t".to_string()))
        );
        assert_eq!(
            feed_item(&AgentStateEvent::Idle {
                outcome: IdleOutcome::Succeeded
            }),
            Some(FeedItem::Boundary(IdleOutcome::Succeeded))
        );
        assert_eq!(
            feed_item(&AgentStateEvent::Error {
                message: "boom".to_string()
            }),
            Some(FeedItem::Error {
                message: "boom".to_string()
            })
        );
        assert_eq!(
            feed_item(&AgentStateEvent::Session {
                session_id: Some("ses_1".to_string()),
                title: Some("title".to_string()),
                model: None,
            }),
            Some(FeedItem::Session {
                title: Some("title".to_string())
            })
        );

        // Deliberately dropped session bookkeeping.
        assert!(feed_item(&AgentStateEvent::TurnStarted).is_none());
        assert!(feed_item(&AgentStateEvent::InputResolved {
            id: "req_1".to_string()
        })
        .is_none());
    }

    #[test]
    fn build_feed_merges_tool_lifecycle_by_id() {
        let events = vec![
            push(
                1,
                AgentStateEvent::ToolStarted {
                    id: "c1".to_string(),
                    name: "read".to_string(),
                    input: serde_json::json!({ "path": "a" }),
                },
            ),
            push(
                2,
                AgentStateEvent::ToolUpdated {
                    id: "c1".to_string(),
                    partial: serde_json::json!("partial"),
                },
            ),
            push(
                3,
                AgentStateEvent::ToolFinished {
                    id: "c1".to_string(),
                    name: "read".to_string(),
                    ok: true,
                    output: Some(serde_json::json!("done")),
                },
            ),
        ];
        let items = build_feed(&events);
        assert_eq!(items.len(), 1);
        match &items[0] {
            FeedItem::Tool {
                name,
                status,
                output,
                input,
                ..
            } => {
                assert_eq!(name, "read");
                assert_eq!(*status, ToolStatus::Ok);
                assert_eq!(*output, Some(serde_json::json!("done")));
                assert_eq!(*input, serde_json::json!({ "path": "a" }));
            }
            other => panic!("expected a tool card, got {other:?}"),
        }
    }

    #[test]
    fn build_feed_drops_a_resolved_input_prompt() {
        let request = InputRequest {
            id: "req_1".to_string(),
            kind: AwaitingInputKind::Permission,
            message: "Allow?".to_string(),
            options: Vec::new(),
            allow_always: false,
        };
        let events = vec![
            push(
                1,
                AgentStateEvent::InputRequested {
                    request: request.clone(),
                },
            ),
            push(
                2,
                AgentStateEvent::InputResolved {
                    id: "req_1".to_string(),
                },
            ),
            push(
                3,
                AgentStateEvent::TextDelta {
                    role: MessageRole::Assistant,
                    text: "ok".to_string(),
                },
            ),
        ];
        let items = build_feed(&events);
        assert!(
            items
                .iter()
                .all(|item| !matches!(item, FeedItem::InputRequest(_))),
            "a resolved prompt must be dropped: {items:?}"
        );
    }

    #[test]
    fn summary_feed_maps_a_run_summary_in_canonical_order() {
        let summary = RunSummary {
            session_id: Some("ses_1".to_string()),
            title: Some("Fix the widget".to_string()),
            text: "all done".to_string(),
            reasoning: "because".to_string(),
            tool_calls: vec![
                ToolCall {
                    id: "c1".to_string(),
                    name: "read".to_string(),
                    input: serde_json::json!({ "path": "a" }),
                    ok: true,
                    output: Some(serde_json::json!("contents")),
                },
                ToolCall {
                    id: "c2".to_string(),
                    name: "bash".to_string(),
                    input: serde_json::json!("rm -rf /"),
                    ok: false,
                    output: None,
                },
            ],
            usage: AgentUsage {
                input_tokens: 3,
                ..Default::default()
            },
            outcome: Some(IdleOutcome::Failed),
            error: Some("boom".to_string()),
        };
        let items = summary_feed(&summary);
        assert!(matches!(
            &items[0],
            FeedItem::Session { title: Some(title) } if title == "Fix the widget"
        ));
        assert!(matches!(
            &items[1],
            FeedItem::Message { role: MessageRole::Assistant, text } if text == "all done"
        ));
        assert!(matches!(&items[2], FeedItem::Reasoning { text } if text == "because"));
        assert!(matches!(
            &items[3],
            FeedItem::Tool { name, status: ToolStatus::Ok, .. } if name == "read"
        ));
        assert!(matches!(
            &items[4],
            FeedItem::Tool { name, status: ToolStatus::Error, .. } if name == "bash"
        ));
        assert!(matches!(&items[5], FeedItem::Usage(_)));
        assert!(matches!(&items[6], FeedItem::Error { message } if message == "boom"));
        assert!(matches!(&items[7], FeedItem::Boundary(IdleOutcome::Failed)));
        assert_eq!(items.len(), 8);
    }

    #[test]
    fn summary_feed_renders_only_the_title_when_nothing_else_reported() {
        // `extract_run_summary` filters a title-only summary out, so this path
        // is only reachable if a summary has content *and* a title.
        let summary = RunSummary {
            title: Some("Only a title".to_string()),
            ..Default::default()
        };
        let items = summary_feed(&summary);
        assert_eq!(items.len(), 1);
        assert!(matches!(
            &items[0],
            FeedItem::Session { title: Some(title) } if title == "Only a title"
        ));
        // A fully empty summary yields nothing.
        assert!(summary_feed(&RunSummary::default()).is_empty());
    }

    #[test]
    fn at_bottom_tracks_the_follow_threshold() {
        // Exactly at the bottom.
        assert!(at_bottom(0.0, 100.0, 100.0));
        assert!(at_bottom(900.0, 100.0, 1000.0));
        // Within the threshold, i.e. `scroll_height - FOLLOW_THRESHOLD`.
        assert!(at_bottom(880.0, 100.0, 1000.0));
        // Scrolled up beyond the threshold pauses following.
        assert!(!at_bottom(870.0, 100.0, 1000.0));
        assert!(!at_bottom(0.0, 100.0, 1000.0));
    }

    #[test]
    fn tool_icon_matches_common_tools_by_name() {
        assert_eq!(tool_icon("bash"), AppIconKind::ToolTerminal);
        assert_eq!(tool_icon("Read"), AppIconKind::ToolRead);
        assert_eq!(tool_icon("grep"), AppIconKind::ToolSearch);
        assert_eq!(tool_icon("some_mystery_tool"), AppIconKind::ToolGeneric);
    }

    #[test]
    fn tool_payloads_unwrap_to_readable_text_blocks() {
        let input = readable_json(&serde_json::json!({ "command": "ls -la" }));
        assert_eq!(
            input,
            Readable::Json(serde_json::json!({ "command": "ls -la" }))
        );

        let output = readable_json(&serde_json::json!({
            "content": [{ "type": "text", "text": "done" }]
        }));
        assert_eq!(output, Readable::Text("done".to_string()));
        assert!(!output.is_empty());
    }

    #[test]
    fn agent_log_mounts_host_side() {
        #[component]
        fn Harness() -> Element {
            let items = use_signal(|| {
                summary_feed(&RunSummary {
                    text: "hello".to_string(),
                    tool_calls: vec![ToolCall {
                        id: "c1".to_string(),
                        name: "read".to_string(),
                        ok: true,
                        output: Some(serde_json::json!("contents")),
                        ..Default::default()
                    }],
                    ..Default::default()
                })
            });
            rsx! {
                AgentLog { items: items() }
            }
        }
        let mut dom = VirtualDom::new(Harness);
        dom.rebuild_in_place();
    }

    #[test]
    fn agent_feed_mounts_host_side() {
        #[component]
        fn Harness() -> Element {
            use_context_provider(AppState::new);
            let events = use_signal(|| {
                vec![
                    push(
                        1,
                        AgentStateEvent::TextDelta {
                            role: MessageRole::Assistant,
                            text: "hello".to_string(),
                        },
                    ),
                    push(
                        2,
                        AgentStateEvent::ToolStarted {
                            id: "c1".to_string(),
                            name: "read".to_string(),
                            input: serde_json::json!({ "path": "a" }),
                        },
                    ),
                    push(
                        3,
                        AgentStateEvent::Idle {
                            outcome: IdleOutcome::Succeeded,
                        },
                    ),
                ]
            });
            let busy = use_signal(|| false);
            let toast = use_signal(|| None::<(ToastKind, String)>);
            let refresh = use_signal(|| 0u64);
            rsx! {
                AgentFeed { events: events(), busy, toast, refresh }
            }
        }
        let mut dom = VirtualDom::new(Harness);
        dom.rebuild_in_place();
    }
}
