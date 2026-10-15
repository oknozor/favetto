//! Live, pretty-formatted agent event feed for the Task view.
//!
//! The daemon normalizes every structured agent into `AgentStateEvent`s and
//! pushes them as non-durable `agent.event` notifications (`tasks.events`
//! backfills after a reload). This module turns that raw stream into a
//! human-readable, expandable feed shown by the Task detail's single output
//! section while the task runs (and from the persisted summary once terminal).
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

/// Lines of a message block shown before the expander.
pub const FEED_COLLAPSED_LINES: usize = 20;

/// The number of pixels from the bottom within which the feed keeps following.
pub const FOLLOW_THRESHOLD: f64 = 24.0;

/// Whether a scroll position is close enough to the bottom to keep following.
pub fn at_bottom(scroll_top: f64, client_height: f64, scroll_height: f64) -> bool {
    scroll_top + client_height >= scroll_height - FOLLOW_THRESHOLD
}

/// What moved the feed's viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollCause {
    /// The component or the layout moved the viewport: the auto-follow scroll
    /// itself, a reflow, content growth, or a row expansion. Never changes the
    /// follow state.
    Programmatic,
    /// An explicit user gesture that moved the viewport toward the top: a wheel
    /// or trackpad scroll up, a downward touch/pointer drag, or an upward
    /// scroll key.
    UserScrolledUp,
    /// A scroll that has come to rest at the bottom.
    ReachedBottom,
}

/// Resolve the auto-follow state after one scroll signal.
///
/// Following is the default and unconditional: only an explicit upward user
/// gesture pauses it, and coming to rest at the bottom resumes it. Every other
/// signal leaves `follow` untouched, so programmatic and layout-driven scrolls
/// can never turn following off.
pub fn follow_after_scroll(follow: bool, cause: ScrollCause) -> bool {
    match cause {
        ScrollCause::Programmatic => follow,
        ScrollCause::UserScrolledUp => false,
        ScrollCause::ReachedBottom => true,
    }
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

/// The full text of a readable block (plain text, or pretty-printed JSON).
fn readable_text(readable: &Readable) -> String {
    match readable {
        Readable::Text(text) => text.clone(),
        Readable::Json(value) => pretty(value),
    }
}

/// The reasoning body to render for the current fold state.
///
/// Reasoning rows fold unconditionally, so a collapsed row renders no body at
/// all (not even a preview) and expanding reveals the whole cleaned text.
fn reasoning_body(text: &str, expanded: bool) -> Option<String> {
    expanded.then(|| strip_ansi(text))
}

/// A tool row's readable bodies and whether it has anything to fold.
struct ToolView {
    /// The readable input (highlighted when JSON).
    input: Readable,
    /// The readable output, if any.
    output: Option<Readable>,
    /// Whether either body has content, so the row earns a disclosure control.
    foldable: bool,
}

impl ToolView {
    /// The input body to render, or `None` while collapsed or empty.
    fn input_shown(&self, expanded: bool) -> Option<String> {
        (expanded && !self.input.is_empty()).then(|| readable_text(&self.input))
    }

    /// The output body to render, or `None` while collapsed or empty.
    fn output_shown(&self, expanded: bool) -> Option<String> {
        self.output
            .as_ref()
            .and_then(|output| (expanded && !output.is_empty()).then(|| readable_text(output)))
    }
}

/// Compute a tool row's readable bodies (raw JSON → readable form).
fn tool_view(input: &serde_json::Value, output: Option<&serde_json::Value>) -> ToolView {
    let input = readable_json(input);
    let output = output.map(readable_json);
    let foldable =
        !input.is_empty() || output.as_ref().is_some_and(|readable| !readable.is_empty());
    ToolView {
        input,
        output,
        foldable,
    }
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

/// The scrollable list of feed rows shared by the live feed and the persisted
/// post-run log.
///
/// The Task detail's merged output section picks the source (live events while
/// the task runs, the durable [`summary_feed`] once terminal) and renders this
/// list; [`FeedItem::InputRequest`] rows keep the inline reply controls, which
/// only the live feed produces.
///
/// Following the newest row is the default and unconditional: every update
/// scrolls to the bottom while following. Only an explicit upward user gesture
/// (wheel/trackpad, touch/pointer drag, or an upward scroll key) pauses it;
/// coming to rest at the bottom or the **Jump to latest** control resumes it.
/// Programmatic and layout-driven scrolls never change the follow state.
#[component]
pub(crate) fn FeedList(
    items: Vec<FeedItem>,
    session_id: String,
    revision: u64,
    busy: Signal<bool>,
    toast: Signal<Option<(ToastKind, String)>>,
    refresh: Signal<u64>,
) -> Element {
    let mut follow = use_signal(|| true);
    let mut element = use_signal(|| None::<web_sys::Element>);
    // Vertical anchor of the in-flight touch/pointer drag, so a downward drag
    // (which scrolls the content up, away from the newest row) can be told
    // apart from a scroll the component itself caused.
    let mut last_touch_y = use_signal(|| None::<f64>);
    let mut last_pointer_y = use_signal(|| None::<f64>);
    let scroll_to_bottom = move || {
        if let Some(el) = element() {
            el.set_scroll_top(el.scroll_height());
        }
    };

    rsx! {
        div { class: "agent-feed-wrap",
            div {
                class: "agent-feed",
                // The scroll region is keyboard-scrollable; `onkeydown` below
                // listens for the upward keys.
                tabindex: "0",
                onmounted: move |evt| {
                    element.set(evt.data().downcast::<web_sys::Element>().cloned());
                    if follow() {
                        scroll_to_bottom();
                    }
                },
                // A scroll only ever *resumes* following, once it comes to rest
                // at the bottom. A non-bottom scroll with no preceding gesture
                // is the component's own auto-follow, a reflow or a row growth,
                // so it is ignored.
                onscroll: move |evt| {
                    let data = evt.data();
                    let at_bottom = at_bottom(
                        data.scroll_top(),
                        f64::from(data.client_height()),
                        f64::from(data.scroll_height()),
                    );
                    if at_bottom {
                        follow.set(follow_after_scroll(follow(), ScrollCause::ReachedBottom));
                    }
                },
                // Explicit upward gestures pause following. A negative wheel
                // delta moves the viewport toward the top.
                onwheel: move |evt| {
                    if evt.delta().strip_units().y < 0.0 {
                        follow.set(follow_after_scroll(follow(), ScrollCause::UserScrolledUp));
                    }
                },
                onkeydown: move |evt| {
                    if matches!(evt.key(), Key::ArrowUp | Key::PageUp | Key::Home) {
                        follow.set(follow_after_scroll(follow(), ScrollCause::UserScrolledUp));
                    }
                },
                ontouchstart: move |evt| {
                    last_touch_y.set(evt.touches().first().map(|touch| touch.client_coordinates().y));
                },
                ontouchmove: move |evt| {
                    let y = evt.touches().first().map(|touch| touch.client_coordinates().y);
                    if let (Some(previous), Some(current)) = (last_touch_y(), y) {
                        // A finger moving down drags the content up.
                        if current > previous {
                            follow
                                .set(follow_after_scroll(follow(), ScrollCause::UserScrolledUp));
                        }
                    }
                    last_touch_y.set(y);
                },
                onpointerdown: move |evt| {
                    last_pointer_y.set(Some(evt.data().client_coordinates().y));
                },
                onpointermove: move |evt| {
                    let data = evt.data();
                    // Only track a drag while a button is held (a scrollbar or
                    // pointer drag).
                    if !data.held_buttons().is_empty() {
                        let y = data.client_coordinates().y;
                        if let Some(previous) = last_pointer_y() {
                            if y > previous {
                                follow.set(follow_after_scroll(
                                    follow(),
                                    ScrollCause::UserScrolledUp,
                                ));
                            }
                        }
                        last_pointer_y.set(Some(y));
                    }
                },
                onpointerup: move |_| last_pointer_y.set(None),
                onpointercancel: move |_| last_pointer_y.set(None),
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
            let body = reasoning_body(&text, expanded());
            rsx! {
                div { class: "feed-item feed-reasoning",
                    span { class: "feed-role role-reasoning", "thinking" }
                    button {
                        class: "feed-toggle",
                        aria_expanded: if expanded() { "true" } else { "false" },
                        onclick: move |_| expanded.set(!expanded()),
                        if expanded() { "Show less" } else { "Show more" }
                    }
                    if let Some(body) = body {
                        pre { class: "feed-text", "{body}" }
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
            // syntax-highlighted. The row folds by default; expanding reveals
            // the full input and output blocks.
            let view = tool_view(&input, output.as_ref());
            let label = if name.is_empty() {
                "tool"
            } else {
                name.as_str()
            };
            let icon = tool_icon(label);
            let card = format!("feed-item feed-tool tool-{}", status.label());
            let chip = format!("tool-chip tool-{}", status.label());
            let is_expanded = expanded();
            let input_shown = view.input_shown(is_expanded);
            let output_shown = view.output_shown(is_expanded);
            rsx! {
                div { class: "{card}",
                    div { class: "feed-tool-head",
                        AppIcon { kind: icon, class: "feed-tool-icon" }
                        span { class: "feed-tool-name", "{label}" }
                        span { class: "{chip}", "{status.label()}" }
                    }
                    if view.foldable {
                        button {
                            class: "feed-toggle",
                            aria_expanded: if is_expanded { "true" } else { "false" },
                            onclick: move |_| expanded.set(!expanded()),
                            if is_expanded { "Show less" } else { "Show more" }
                        }
                    }
                    if let Some(shown) = &input_shown {
                        {readable_pre("feed-tool-input", &view.input, shown)}
                    }
                    if let Some(readable) = &view.output {
                        if let Some(shown) = &output_shown {
                            {readable_pre("feed-tool-output", readable, shown)}
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
    use dioxus::core::{AttributeValue, ElementId, Mutation, Mutations};
    use dioxus::html::{SerializedScrollData, SerializedWheelData};
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
        // Exactly on the threshold, i.e. `scroll_height - FOLLOW_THRESHOLD`.
        assert!(at_bottom(1000.0 - FOLLOW_THRESHOLD, 0.0, 1000.0));
        // One pixel further up is already outside the threshold.
        assert!(!at_bottom(1000.0 - FOLLOW_THRESHOLD - 1.0, 0.0, 1000.0));
        // Within the threshold.
        assert!(at_bottom(880.0, 100.0, 1000.0));
        // Scrolled up beyond the threshold pauses following.
        assert!(!at_bottom(870.0, 100.0, 1000.0));
        assert!(!at_bottom(0.0, 100.0, 1000.0));
    }

    #[test]
    fn follow_after_scroll_tracks_explicit_intent() {
        // Programmatic and layout-driven scrolls never change the state: this
        // is the core regression guard for the smooth-scroll event burst.
        assert!(follow_after_scroll(true, ScrollCause::Programmatic));
        assert!(!follow_after_scroll(false, ScrollCause::Programmatic));
        // An explicit upward gesture pauses following (and keeps it paused).
        assert!(!follow_after_scroll(true, ScrollCause::UserScrolledUp));
        assert!(!follow_after_scroll(false, ScrollCause::UserScrolledUp));
        // Coming to rest at the bottom resumes following.
        assert!(follow_after_scroll(false, ScrollCause::ReachedBottom));
        assert!(follow_after_scroll(true, ScrollCause::ReachedBottom));
    }

    #[test]
    fn follow_after_scroll_is_idempotent_for_programmatic_scrolls() {
        // A smooth-scroll animation (or any run of layout scrolls) is a burst
        // of programmatic signals; none of them may pause following.
        let mut follow = true;
        for _ in 0..10 {
            follow = follow_after_scroll(follow, ScrollCause::Programmatic);
        }
        assert!(follow, "programmatic scrolls must never pause following");
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

    /// Stringify the built-in [`AttributeValue`] variants for DOM assertions.
    fn attribute_text(value: &AttributeValue) -> Option<String> {
        match value {
            AttributeValue::Text(text) => Some(text.clone()),
            AttributeValue::Bool(value) => Some(value.to_string()),
            AttributeValue::Int(value) => Some(value.to_string()),
            AttributeValue::Float(value) => Some(value.to_string()),
            _ => None,
        }
    }

    #[test]
    fn reasoning_folds_by_default_and_reveals_the_full_text() {
        // A collapsed row renders no body at all - not even a short preview.
        assert_eq!(reasoning_body("line 1\nline 2", false), None);

        // Expanding reveals the whole text, even past the message clamp.
        let long = (0..FEED_COLLAPSED_LINES + 5)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let body = reasoning_body(&long, true).expect("expanded reasoning has a body");
        assert_eq!(body, long);
        assert_eq!(body.lines().count(), FEED_COLLAPSED_LINES + 5);

        // ANSI escapes are stripped from the revealed body.
        assert_eq!(
            reasoning_body("\u{1b}[31mred\u{1b}[0m", true),
            Some("red".to_string())
        );
    }

    #[test]
    fn tool_bodies_fold_by_default_and_reveal_the_full_blocks() {
        let view = tool_view(
            &serde_json::json!({ "path": "a" }),
            Some(&serde_json::json!("done")),
        );
        assert!(view.foldable);
        assert_eq!(view.input_shown(false), None);
        assert_eq!(view.output_shown(false), None);
        assert!(view.input_shown(true).is_some());
        assert_eq!(view.output_shown(true), Some("done".to_string()));

        // A row with no bodies earns no disclosure control.
        let empty = tool_view(&serde_json::Value::Null, None);
        assert!(!empty.foldable);
        assert_eq!(empty.input_shown(true), None);
        assert_eq!(empty.output_shown(true), None);
    }

    #[test]
    fn feed_rows_fold_by_default_in_the_rendered_dom() {
        #[component]
        fn Harness() -> Element {
            use_context_provider(AppState::new);
            let items = use_signal(|| {
                vec![
                    FeedItem::Reasoning {
                        text: "REASONING_BODY_SENTINEL".to_string(),
                    },
                    FeedItem::Tool {
                        id: "c1".to_string(),
                        name: "read".to_string(),
                        input: serde_json::json!({ "path": "INPUT_BODY_SENTINEL" }),
                        status: ToolStatus::Ok,
                        output: Some(serde_json::json!("OUTPUT_BODY_SENTINEL")),
                    },
                ]
            });
            let busy = use_signal(|| false);
            let toast = use_signal(|| None::<(ToastKind, String)>);
            let refresh = use_signal(|| 0u64);
            rsx! {
                FeedList {
                    items: items(),
                    session_id: "ses_1".to_string(),
                    revision: 2,
                    busy,
                    toast,
                    refresh,
                }
            }
        }

        let mut dom = VirtualDom::new(Harness);
        let mutations = dom.rebuild_to_vec();

        let mut text = String::new();
        let mut collapsed_toggles = 0usize;
        for edit in &mutations.edits {
            match edit {
                Mutation::CreateTextNode { value, .. } | Mutation::SetText { value, .. } => {
                    text.push_str(value);
                    text.push('\n');
                }
                Mutation::SetAttribute { name, value, .. }
                    if *name == "aria-expanded"
                        && attribute_text(value).as_deref() == Some("false") =>
                {
                    collapsed_toggles += 1;
                }
                _ => {}
            }
        }

        // The header stays visible while folded: tool name and status chip.
        assert!(
            text.contains("read"),
            "tool name missing from folded row: {text}"
        );
        assert!(
            text.contains("ok"),
            "status chip missing from folded row: {text}"
        );
        // ...but no body is rendered.
        assert!(
            !text.contains("REASONING_BODY_SENTINEL"),
            "reasoning body leaked while folded: {text}"
        );
        assert!(
            !text.contains("INPUT_BODY_SENTINEL"),
            "tool input leaked while folded: {text}"
        );
        assert!(
            !text.contains("OUTPUT_BODY_SENTINEL"),
            "tool output leaked while folded: {text}"
        );
        // One disclosure control per foldable row, both collapsed by default.
        assert_eq!(
            collapsed_toggles, 2,
            "expected two collapsed toggles: {text}"
        );
    }

    #[test]
    fn feed_list_mounts_host_side() {
        #[component]
        fn Harness() -> Element {
            use_context_provider(AppState::new);
            let items = use_signal(|| {
                vec![
                    FeedItem::Message {
                        role: MessageRole::Assistant,
                        text: "hello".to_string(),
                    },
                    FeedItem::Reasoning {
                        text: "considering".to_string(),
                    },
                    FeedItem::Tool {
                        id: "c1".to_string(),
                        name: "read".to_string(),
                        input: serde_json::json!({ "path": "a" }),
                        status: ToolStatus::Running,
                        output: None,
                    },
                    FeedItem::Boundary(IdleOutcome::Succeeded),
                ]
            });
            let busy = use_signal(|| false);
            let toast = use_signal(|| None::<(ToastKind, String)>);
            let refresh = use_signal(|| 0u64);
            rsx! {
                FeedList {
                    items: items(),
                    session_id: "ses_1".to_string(),
                    revision: 3,
                    busy,
                    toast,
                    refresh,
                }
            }
        }
        let mut dom = VirtualDom::new(Harness);
        dom.rebuild_in_place();
    }

    // -- Intent-based follow: host-side synthetic-event coverage --------------
    //
    // These dispatch real DOM events through the same serialized converter the
    // renderer uses, so the `onscroll`/`onwheel` wiring is exercised end to end
    // against the rendered "Jump to latest" control.

    /// Install the host-side event converter the native/wasm renderers register
    /// before delivering DOM events.
    fn install_event_converter() {
        dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
    }

    /// Dispatch a synthetic event carrying `data` to `element`.
    fn dispatch<E: 'static>(dom: &mut VirtualDom, name: &str, element: ElementId, data: E) {
        let payload = dioxus::html::PlatformEventData::new(Box::new(data));
        let event: Event<dyn std::any::Any> = Event::new(std::rc::Rc::new(payload), true);
        // `NewEventListener`/`handle_event` use the bare event name (`scroll`),
        // without the `on` attribute prefix.
        dom.runtime().handle_event(name, event, element);
    }

    /// The first element that registered a listener for `name`.
    fn listener_target(mutations: &Mutations, name: &str) -> ElementId {
        mutations
            .edits
            .iter()
            .find_map(|mutation| match mutation {
                Mutation::NewEventListener {
                    name: registered,
                    id,
                } if registered == name => Some(*id),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no `{name}` listener in: {:?}", mutations.edits))
    }

    /// A tiny DOM model tracking the "Jump to latest" control.
    ///
    /// The Dioxus mutation recorder hoists the button's static "Jump to latest"
    /// label into a template, so the text never appears as a text-node edit.
    /// The control's `click` listener id is the reliable signal: mounting the
    /// button registers it, and replacing the button removes it.
    #[derive(Default)]
    struct FollowDom {
        click_listeners: std::collections::HashSet<ElementId>,
    }

    impl FollowDom {
        fn apply(&mut self, mutations: &Mutations) {
            for edit in &mutations.edits {
                match edit {
                    Mutation::NewEventListener { name, id } if name == "click" => {
                        self.click_listeners.insert(*id);
                    }
                    Mutation::RemoveEventListener { name, id } if name == "click" => {
                        self.click_listeners.remove(id);
                    }
                    Mutation::Remove { id } | Mutation::ReplaceWith { id, .. } => {
                        self.click_listeners.remove(id);
                    }
                    _ => {}
                }
            }
        }

        /// True while the "Jump to latest" control is on screen, i.e. following
        /// is paused.
        fn paused(&self) -> bool {
            !self.click_listeners.is_empty()
        }
    }

    fn scroll_data(
        scroll_top: f64,
        client_height: i32,
        scroll_height: i32,
    ) -> SerializedScrollData {
        SerializedScrollData {
            scroll_top,
            scroll_left: 0.0,
            scroll_width: 0,
            scroll_height,
            client_width: 0,
            client_height,
        }
    }

    fn wheel_data(delta_y: f64) -> SerializedWheelData {
        SerializedWheelData {
            mouse: Default::default(),
            delta_mode: 0,
            delta_x: 0.0,
            delta_y,
            delta_z: 0.0,
        }
    }

    #[test]
    fn feed_list_follows_until_an_explicit_user_scroll() {
        #[component]
        fn Harness() -> Element {
            use_context_provider(AppState::new);
            let items = use_signal(|| {
                vec![
                    FeedItem::Message {
                        role: MessageRole::Assistant,
                        text: "hello".to_string(),
                    },
                    FeedItem::Boundary(IdleOutcome::Succeeded),
                ]
            });
            let busy = use_signal(|| false);
            let toast = use_signal(|| None::<(ToastKind, String)>);
            let refresh = use_signal(|| 0u64);
            rsx! {
                FeedList {
                    items: items(),
                    session_id: "ses_1".to_string(),
                    revision: 1,
                    busy,
                    toast,
                    refresh,
                }
            }
        }

        install_event_converter();
        let mut dom = VirtualDom::new(Harness);
        let initial = dom.rebuild_to_vec();
        let mut follow_dom = FollowDom::default();
        follow_dom.apply(&initial);
        // Following is the default: no Jump control on mount.
        assert!(!follow_dom.paused(), "following must be the default");

        let scroll_target = listener_target(&initial, "scroll");
        let wheel_target = listener_target(&initial, "wheel");

        // A non-bottom scroll with no gesture (our own auto-follow, a reflow or
        // a row growth) must not pause following.
        dispatch(
            &mut dom,
            "scroll",
            scroll_target,
            scroll_data(0.0, 100, 1000),
        );
        follow_dom.apply(&dom.render_immediate_to_vec());
        assert!(
            !follow_dom.paused(),
            "a programmatic scroll must not pause following"
        );

        // An explicit wheel-up pauses following.
        dispatch(&mut dom, "wheel", wheel_target, wheel_data(-120.0));
        follow_dom.apply(&dom.render_immediate_to_vec());
        assert!(
            follow_dom.paused(),
            "an upward wheel gesture must pause following"
        );

        // Scrolling back to the bottom resumes following.
        dispatch(
            &mut dom,
            "scroll",
            scroll_target,
            scroll_data(900.0, 100, 1000),
        );
        follow_dom.apply(&dom.render_immediate_to_vec());
        assert!(
            !follow_dom.paused(),
            "reaching the bottom must resume following"
        );

        // A downward wheel never pauses.
        dispatch(&mut dom, "wheel", wheel_target, wheel_data(120.0));
        follow_dom.apply(&dom.render_immediate_to_vec());
        assert!(
            !follow_dom.paused(),
            "a downward wheel must not pause following"
        );
    }
}
