//! Ordered, bounded feed model for an active ACP session.
//!
//! ACP delivers one conversation as a stream of deltas and tool-call snapshots.
//! Rendering that stream as terminal text loses the hierarchy (and can interpret
//! agent-authored escape sequences). This module keeps the stream structured:
//! adjacent text/reasoning deltas coalesce, and tool calls are updated in place
//! by their ACP correlation id. The UI renders the resulting items as prose,
//! muted collapsible thoughts, and compact foldable tool cards.

use std::collections::{HashMap, VecDeque};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use favetto_core::model::{
    AgentStateEvent, ContentRole, IdleOutcome, MessageContent, MessageRole, ToolCallDetail,
    ToolCallStatus, ToolContent, ToolKind, ToolLocation,
};

use super::sanitize::{line as clean_line, text as clean_text};
use super::theme::Theme;

/// Upper bound on retained feed items. Older items are dropped first, so a long
/// session cannot grow the TUI's memory or redraw cost without bound.
pub const MAX_FEED_ITEMS: usize = 512;

/// One ordered entry in the ACP feed.
#[derive(Debug, Clone, PartialEq)]
pub struct FeedItem {
    /// Stable per-feed id, used by the UI to keep a fold selection while new
    /// events arrive and old items are pruned.
    pub id: u64,
    pub kind: FeedKind,
}

impl FeedItem {
    /// Whether the UI can fold this row (thoughts and tool details).
    pub fn is_foldable(&self) -> bool {
        matches!(self.kind, FeedKind::Thinking { .. } | FeedKind::Tool(_))
    }
}

/// The display shape of a feed entry.
#[derive(Debug, Clone, PartialEq)]
pub enum FeedKind {
    /// Prose from the assistant, user, system, or a legacy tool message.
    Message { role: MessageRole, text: String },
    /// Muted chain-of-thought, collapsed by default. `streaming` marks the
    /// trailing thought run while it is still being appended to.
    Thinking {
        text: String,
        streaming: bool,
        expanded: bool,
    },
    /// A non-text message block, rendered as a safe, non-interactive
    /// placeholder. Binary payloads never reach the feed.
    ContentBlock {
        role: ContentRole,
        content: MessageContent,
    },
    /// A tool invocation, updated in place by ACP id.
    Tool(Box<ToolCard>),
    /// A session-level error.
    Error { message: String },
    /// A turn/run boundary.
    Boundary { outcome: IdleOutcome },
}

/// The cumulative, renderable state of one tool call.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCard {
    pub id: String,
    pub name: String,
    pub title: Option<String>,
    pub kind: ToolKind,
    pub status: ToolCallStatus,
    pub input: Option<serde_json::Value>,
    pub output: Option<serde_json::Value>,
    pub content: Vec<ToolContent>,
    pub locations: Vec<ToolLocation>,
    pub expanded: bool,
}

impl ToolCard {
    fn new(id: String) -> Self {
        Self {
            id,
            name: String::new(),
            title: None,
            kind: ToolKind::Other,
            status: ToolCallStatus::Pending,
            input: None,
            output: None,
            content: Vec::new(),
            locations: Vec::new(),
            expanded: false,
        }
    }

    /// A readable name, falling back from the programmatic name to the title.
    pub fn display_name(&self) -> &str {
        if !self.name.trim().is_empty() {
            &self.name
        } else {
            self.title
                .as_deref()
                .filter(|title| !title.trim().is_empty())
                .unwrap_or("tool")
        }
    }

    /// A compact single-line summary for the collapsed row.
    pub fn summary(&self) -> Option<String> {
        if let Some(title) = self
            .title
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty() && *title != self.display_name())
        {
            return Some(title.to_string());
        }
        if let Some(input) = self.input.as_ref().and_then(input_summary) {
            return Some(input);
        }
        if let Some(location) = self.locations.first() {
            return Some(format_location(location));
        }
        self.output
            .as_ref()
            .and_then(input_summary)
            .or_else(|| self.content.iter().find_map(content_summary))
    }

    /// The textual status. Colour is never the only status signal.
    pub fn status_label(&self) -> &'static str {
        match self.status {
            ToolCallStatus::Pending => "pending",
            ToolCallStatus::InProgress => "running",
            ToolCallStatus::Completed => "completed",
            ToolCallStatus::Failed => "failed",
            ToolCallStatus::Unknown => "unknown",
        }
    }

    /// A single-cell icon chosen from the ACP tool kind. `Other` is the neutral
    /// fallback for an absent or unrecognised kind.
    pub fn kind_icon(&self) -> &'static str {
        match self.kind {
            ToolKind::Read => "▤",
            ToolKind::Edit => "✎",
            ToolKind::Delete => "✕",
            ToolKind::Move => "→",
            ToolKind::Search => "⌕",
            ToolKind::Execute => "$",
            ToolKind::Think => "…",
            ToolKind::Fetch => "↓",
            ToolKind::SwitchMode => "⇄",
            ToolKind::Other => "•",
        }
    }

    /// Whether expanding this row would show anything.
    pub fn has_details(&self) -> bool {
        self.input.as_ref().is_some_and(|v| !v.is_null())
            || self.output.as_ref().is_some_and(|v| !v.is_null())
            || !self.content.is_empty()
            || !self.locations.is_empty()
    }

    fn status_style(&self, theme: &Theme) -> Style {
        let color = match self.status {
            ToolCallStatus::Pending => theme.warning(),
            ToolCallStatus::InProgress => theme.accent(),
            ToolCallStatus::Completed => theme.success(),
            ToolCallStatus::Failed => theme.danger(),
            ToolCallStatus::Unknown => theme.muted(),
        };
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    }
}

/// An ordered, bounded feed. Tool updates merge into the original card by id,
/// so interleaved text and updates never duplicate a tool row.
#[derive(Debug, Clone, Default)]
pub struct AgentFeed {
    items: VecDeque<FeedItem>,
    /// ACP tool-call id → stable feed item id.
    tools: HashMap<String, u64>,
    next_item_id: u64,
}

impl AgentFeed {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop every item (a new session starts with a clean feed).
    pub fn clear(&mut self) {
        self.items.clear();
        self.tools.clear();
        self.next_item_id = 0;
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[cfg(test)]
    pub fn items(&self) -> impl Iterator<Item = &FeedItem> {
        self.items.iter()
    }

    /// Stable ids of every foldable row, in feed order.
    pub fn foldable_ids(&self) -> Vec<u64> {
        self.items
            .iter()
            .filter(|item| item.is_foldable())
            .map(|item| item.id)
            .collect()
    }

    /// ACP ids of every tool card, in feed order (test helper).
    #[cfg(test)]
    pub fn tool_ids(&self) -> Vec<&str> {
        self.items
            .iter()
            .filter_map(|item| match &item.kind {
                FeedKind::Tool(tool) => Some(tool.id.as_str()),
                _ => None,
            })
            .collect()
    }

    pub fn contains_item(&self, item_id: u64) -> bool {
        self.items.iter().any(|item| item.id == item_id)
    }

    /// Fold/unfold a thought or tool row. Returns whether the id was found.
    pub fn toggle_item(&mut self, item_id: u64) -> bool {
        let Some(item) = self.items.iter_mut().find(|item| item.id == item_id) else {
            return false;
        };
        match &mut item.kind {
            FeedKind::Thinking { expanded, .. } => *expanded = !*expanded,
            FeedKind::Tool(tool) => tool.expanded = !tool.expanded,
            _ => return false,
        }
        true
    }

    /// Whether the given tool card is expanded (test helper).
    #[cfg(test)]
    pub fn tool_expanded(&self, id: &str) -> Option<bool> {
        self.items.iter().find_map(|item| match &item.kind {
            FeedKind::Tool(tool) if tool.id == id => Some(tool.expanded),
            _ => None,
        })
    }

    /// Stop the trailing thought's streaming marker (e.g. when the session exits).
    pub fn finish_streaming(&mut self) {
        if let Some(FeedItem {
            kind: FeedKind::Thinking { streaming, .. },
            ..
        }) = self.items.back_mut()
        {
            *streaming = false;
        }
    }

    /// Fold one normalized ACP event into the feed, preserving event order.
    pub fn apply(&mut self, event: &AgentStateEvent) {
        // Only a consecutive reasoning run keeps the streaming marker. Any other
        // event closes it before it is rendered.
        if !matches!(event, AgentStateEvent::ReasoningDelta { .. }) {
            self.finish_streaming();
        }

        match event {
            AgentStateEvent::TextDelta { role, text } => {
                let text = clean_text(text);
                if let Some(FeedItem {
                    kind:
                        FeedKind::Message {
                            role: current,
                            text: current_text,
                        },
                    ..
                }) = self.items.back_mut()
                {
                    if current == role {
                        current_text.push_str(&text);
                        return;
                    }
                }
                self.push(FeedKind::Message { role: *role, text });
            }
            AgentStateEvent::ContentBlock { role, content } => {
                self.push(FeedKind::ContentBlock {
                    role: *role,
                    content: content.clone(),
                });
            }
            AgentStateEvent::ReasoningDelta { text } => {
                let text = clean_text(text);
                if let Some(FeedItem {
                    kind:
                        FeedKind::Thinking {
                            text: current,
                            streaming,
                            ..
                        },
                    ..
                }) = self.items.back_mut()
                {
                    current.push_str(&text);
                    *streaming = true;
                } else {
                    self.push(FeedKind::Thinking {
                        text,
                        streaming: true,
                        expanded: false,
                    });
                }
            }
            AgentStateEvent::ToolStarted {
                id,
                name,
                input,
                detail,
            } => {
                self.start_tool(id, name, input, detail.as_deref());
            }
            AgentStateEvent::ToolUpdated {
                id,
                partial,
                detail,
            } => {
                self.update_tool(id, partial, detail.as_deref());
            }
            AgentStateEvent::ToolFinished {
                id,
                name,
                ok,
                output,
                detail,
            } => {
                self.finish_tool(id, name, *ok, output.as_ref(), detail.as_deref());
            }
            AgentStateEvent::Error { message } => {
                self.push(FeedKind::Error {
                    message: clean_text(message),
                });
            }
            AgentStateEvent::Idle { outcome } => {
                self.push(FeedKind::Boundary { outcome: *outcome });
            }
            // Session identity, title, usage, prompts, and turn starts are not
            // feed rows. They continue to drive the status bar and reply flow.
            _ => {}
        }
    }

    fn push(&mut self, kind: FeedKind) -> u64 {
        let id = self.next_item_id;
        self.next_item_id = self.next_item_id.wrapping_add(1);
        self.items.push_back(FeedItem { id, kind });
        while self.items.len() > MAX_FEED_ITEMS {
            if let Some(dropped) = self.items.pop_front() {
                if let FeedKind::Tool(tool) = dropped.kind {
                    if self.tools.get(&tool.id) == Some(&dropped.id) {
                        self.tools.remove(&tool.id);
                    }
                }
            }
        }
        id
    }

    fn tool_index(&self, id: &str) -> Option<usize> {
        let item_id = *self.tools.get(id)?;
        self.items.iter().position(|item| item.id == item_id)
    }

    fn start_tool(
        &mut self,
        id: &str,
        name: &str,
        input: &serde_json::Value,
        detail: Option<&ToolCallDetail>,
    ) {
        let index = self.tool_index(id);
        let item = match index {
            Some(index) => &mut self.items[index],
            None => {
                let item_id = self.push(FeedKind::Tool(Box::new(ToolCard::new(id.to_string()))));
                self.tools.insert(id.to_string(), item_id);
                self.items
                    .iter_mut()
                    .find(|item| item.id == item_id)
                    .expect("just inserted feed item")
            }
        };
        let FeedKind::Tool(tool) = &mut item.kind else {
            return;
        };

        if !name.trim().is_empty() {
            tool.name = clean_line(name);
        }
        if let Some(detail) = detail {
            merge_detail(tool, detail);
        }
        if tool.name.trim().is_empty() {
            tool.name = tool
                .title
                .clone()
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| "tool".to_string());
        }
        if !input.is_null() && tool.input.as_ref().is_none_or(serde_json::Value::is_null) {
            tool.input = Some(input.clone());
        }
        // A plain legacy `ToolStarted` means the call is already running. An ACP
        // detail supplies its own (often `pending`) status.
        if detail.and_then(|d| d.status).is_none() && tool.status == ToolCallStatus::Pending {
            tool.status = ToolCallStatus::InProgress;
        }
    }

    fn update_tool(
        &mut self,
        id: &str,
        partial: &serde_json::Value,
        detail: Option<&ToolCallDetail>,
    ) {
        let index = self.tool_index(id);
        let item = match index {
            Some(index) => &mut self.items[index],
            None => {
                let item_id = self.push(FeedKind::Tool(Box::new(ToolCard::new(id.to_string()))));
                self.tools.insert(id.to_string(), item_id);
                self.items
                    .iter_mut()
                    .find(|item| item.id == item_id)
                    .expect("just inserted feed item")
            }
        };
        let FeedKind::Tool(tool) = &mut item.kind else {
            return;
        };

        if let Some(detail) = detail {
            merge_detail(tool, detail);
        } else {
            if tool.name.trim().is_empty() {
                tool.name = "tool".to_string();
            }
            if tool.status == ToolCallStatus::Pending {
                tool.status = ToolCallStatus::InProgress;
            }
            if !partial.is_null() {
                tool.output = Some(partial.clone());
            }
        }
        if tool.name.trim().is_empty() {
            tool.name = tool
                .title
                .clone()
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| "tool".to_string());
        }
        if detail.is_some()
            && detail.and_then(|d| d.status).is_none()
            && tool.status == ToolCallStatus::Pending
        {
            // An update without an explicit status still means the tool is live.
            tool.status = ToolCallStatus::InProgress;
        }
    }

    fn finish_tool(
        &mut self,
        id: &str,
        name: &str,
        ok: bool,
        output: Option<&serde_json::Value>,
        detail: Option<&ToolCallDetail>,
    ) {
        let index = self.tool_index(id);
        let item = match index {
            Some(index) => &mut self.items[index],
            None => {
                let item_id = self.push(FeedKind::Tool(Box::new(ToolCard::new(id.to_string()))));
                self.tools.insert(id.to_string(), item_id);
                self.items
                    .iter_mut()
                    .find(|item| item.id == item_id)
                    .expect("just inserted feed item")
            }
        };
        let FeedKind::Tool(tool) = &mut item.kind else {
            return;
        };

        if !name.trim().is_empty() {
            tool.name = clean_line(name);
        }
        if let Some(detail) = detail {
            merge_detail(tool, detail);
        }
        if let Some(output) = output {
            if !output.is_null() {
                tool.output = Some(output.clone());
            }
        }
        if tool.name.trim().is_empty() {
            tool.name = tool
                .title
                .clone()
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| "tool".to_string());
        }
        tool.status = if ok {
            ToolCallStatus::Completed
        } else {
            ToolCallStatus::Failed
        };
    }

    /// Render the feed as styled lines. `selected` marks a foldable row without
    /// changing its fold state, and the returned range locates that row so the
    /// UI can keep it visible.
    pub fn render(&self, theme: &Theme, selected: Option<u64>) -> FeedLines {
        let mut lines = Vec::new();
        let mut selected_range = None;

        for (index, item) in self.items.iter().enumerate() {
            if index > 0 {
                lines.push(Line::default());
            }
            let start = lines.len();
            match &item.kind {
                FeedKind::Message { role, text } => push_message(&mut lines, *role, text, theme),
                FeedKind::Thinking {
                    text,
                    streaming,
                    expanded,
                } => push_thinking(
                    &mut lines,
                    text,
                    *streaming,
                    *expanded,
                    theme,
                    selected == Some(item.id),
                ),
                FeedKind::ContentBlock { role, content } => {
                    push_content_block(&mut lines, *role, content, theme)
                }
                FeedKind::Tool(tool) => {
                    push_tool(&mut lines, tool, theme, selected == Some(item.id))
                }
                FeedKind::Error { message } => push_error(&mut lines, message, theme),
                FeedKind::Boundary { outcome } => push_boundary(&mut lines, *outcome, theme),
            };
            if selected == Some(item.id) && item.is_foldable() {
                selected_range = Some((start, lines.len()));
            }
        }

        if lines.is_empty() {
            lines.push(Line::from(Span::styled(
                "Waiting for the agent's first event…",
                theme.muted_style(),
            )));
        }

        // Defense in depth: every span leaving this module is already built from
        // sanitized strings, but a final pass makes a missed field harmless and
        // guarantees no control character can reach a ratatui cell.
        let lines = lines.into_iter().map(clean_spans).collect();

        FeedLines {
            lines,
            selected_range,
        }
    }
}

/// Rebuild a line with each span sanitized for a single terminal row. The
/// sanitizer preserves ordinary Unicode and Markdown styling.
fn clean_spans(line: Line<'static>) -> Line<'static> {
    let spans = line
        .spans
        .into_iter()
        .map(|span| {
            let content = clean_line(&span.content);
            if content == span.content.as_ref() {
                span
            } else {
                Span::styled(content, span.style)
            }
        })
        .collect();
    Line {
        spans,
        style: line.style,
        alignment: line.alignment,
    }
}

/// Rendered feed rows plus the logical line range of the selected row.
pub struct FeedLines {
    pub lines: Vec<Line<'static>>,
    pub selected_range: Option<(usize, usize)>,
}

fn merge_detail(tool: &mut ToolCard, detail: &ToolCallDetail) {
    if let Some(name) = &detail.name {
        tool.name = clean_line(name);
    }
    if let Some(title) = &detail.title {
        tool.title = Some(clean_line(title));
    }
    if let Some(kind) = detail.kind {
        tool.kind = kind;
    }
    if let Some(status) = detail.status {
        merge_status(tool, status);
    }
    if let Some(input) = &detail.input {
        tool.input = Some(input.clone());
    }
    if let Some(output) = &detail.output {
        tool.output = Some(output.clone());
    }
    // `detail` is a cumulative snapshot, not a patch: an empty collection is an
    // explicit clear and must replace what the card held before.
    tool.content = detail.content.clone();
    tool.locations = detail.locations.clone();
}

/// A tool lifecycle only moves forward. A late or repeated snapshot must not
/// downgrade a terminal (or running) card back to `pending`.
fn merge_status(tool: &mut ToolCard, status: ToolCallStatus) {
    fn rank(status: ToolCallStatus) -> u8 {
        match status {
            ToolCallStatus::Pending | ToolCallStatus::Unknown => 0,
            ToolCallStatus::InProgress => 1,
            ToolCallStatus::Completed | ToolCallStatus::Failed => 2,
        }
    }
    if rank(status) >= rank(tool.status) {
        tool.status = status;
    }
}

fn push_message(out: &mut Vec<Line<'static>>, role: MessageRole, text: &str, theme: &Theme) {
    let text = clean_text(text);
    match role {
        MessageRole::User => {
            let mut lines = text.lines();
            let first = lines.next().unwrap_or_default();
            out.push(Line::from(vec![
                Span::styled(
                    "You  ",
                    Style::default()
                        .fg(theme.accent())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(first.to_string(), Style::default().fg(theme.fg())),
            ]));
            for line in lines {
                out.push(Line::from(vec![
                    Span::raw("     "),
                    Span::styled(line.to_string(), Style::default().fg(theme.fg())),
                ]));
            }
        }
        MessageRole::Assistant => {
            // Assistant prose is Markdown. Tool names, paths, commands, and other
            // protocol fields are rendered by their own plain-text paths below.
            out.extend(super::markdown::render(&text, theme));
        }
        MessageRole::System | MessageRole::Tool => {
            let label = if role == MessageRole::System {
                "System"
            } else {
                "Tool"
            };
            out.push(Line::from(Span::styled(
                label,
                Style::default()
                    .fg(theme.muted())
                    .add_modifier(Modifier::BOLD),
            )));
            for line in text.lines() {
                out.push(Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(theme.muted()),
                )));
            }
        }
    }
}

fn push_thinking(
    out: &mut Vec<Line<'static>>,
    text: &str,
    streaming: bool,
    expanded: bool,
    theme: &Theme,
    selected: bool,
) {
    let mut header = vec![
        Span::styled(
            if expanded { "▾ " } else { "▸ " },
            Style::default().fg(theme.muted()),
        ),
        Span::styled(
            "Thinking",
            Style::default()
                .fg(theme.muted())
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if streaming {
        header.push(Span::styled(
            " · streaming…",
            Style::default()
                .fg(theme.muted())
                .add_modifier(Modifier::ITALIC),
        ));
    }
    let mut line = Line::from(header);
    if selected {
        line.style = selected_line_style(theme);
    }
    out.push(line);
    if expanded {
        for line in text.lines() {
            out.push(Line::from(Span::styled(
                format!("  {line}"),
                Style::default()
                    .fg(theme.muted())
                    .add_modifier(Modifier::ITALIC),
            )));
        }
    }
}

fn push_content_block(
    out: &mut Vec<Line<'static>>,
    role: ContentRole,
    content: &MessageContent,
    theme: &Theme,
) {
    let label = match role {
        ContentRole::User => "You",
        ContentRole::Assistant => "Assistant",
        ContentRole::Thought => "Thinking",
    };
    out.push(Line::from(vec![
        Span::styled(
            format!("{label}  "),
            Style::default()
                .fg(theme.accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            clean_line(&content.placeholder()),
            Style::default()
                .fg(theme.muted())
                .add_modifier(Modifier::ITALIC),
        ),
    ]));
}

/// Highlight the selected row. A background is enough in a colour theme; the
/// monochrome theme (`Reset` selection colours) also reverses the cells so
/// keyboard focus stays visible without colour.
fn selected_line_style(theme: &Theme) -> Style {
    let style = Style::default()
        .fg(theme.selected_fg())
        .bg(theme.selected_bg())
        .add_modifier(Modifier::BOLD);
    if theme.selected_bg() == Color::Reset {
        style.add_modifier(Modifier::REVERSED)
    } else {
        style
    }
}

fn push_tool(out: &mut Vec<Line<'static>>, tool: &ToolCard, theme: &Theme, selected: bool) {
    let disclosure = if !tool.has_details() {
        "  "
    } else if tool.expanded {
        "▾ "
    } else {
        "▸ "
    };
    let mut header = vec![
        Span::styled(disclosure, Style::default().fg(theme.muted())),
        Span::styled(
            format!("{} ", tool.kind_icon()),
            Style::default().fg(theme.accent()),
        ),
        Span::styled(
            clean_line(tool.display_name()),
            Style::default().fg(theme.fg()).add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(summary) = tool.summary() {
        header.push(Span::styled(
            format!(" — {}", clean_line(&summary)),
            Style::default().fg(theme.muted()),
        ));
    }
    header.push(Span::styled(
        format!(" · {}", tool.status_label()),
        tool.status_style(theme),
    ));
    let mut line = Line::from(header);
    if selected {
        line.style = selected_line_style(theme);
    }
    out.push(line);

    if !tool.expanded || !tool.has_details() {
        return;
    }

    if let Some(input) = &tool.input {
        if !input.is_null() {
            push_detail(out, "Input", super::json::render(input, theme));
        }
    }
    if let Some(output) = &tool.output {
        if !output.is_null() {
            push_detail(out, "Output", super::json::render(output, theme));
        }
    }
    if !tool.content.is_empty() {
        push_detail(out, "Content", content_lines(&tool.content, theme));
    }
    if !tool.locations.is_empty() {
        let lines = tool
            .locations
            .iter()
            .map(|location| {
                Line::from(Span::styled(
                    format_location(location),
                    Style::default().fg(theme.fg()),
                ))
            })
            .collect();
        push_detail(out, "Locations", lines);
    }
}

fn push_detail(out: &mut Vec<Line<'static>>, label: &str, lines: Vec<Line<'static>>) {
    out.push(Line::from(Span::styled(
        format!("  {label}"),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    for line in lines {
        let style = line.style;
        let mut spans = vec![Span::raw("    ")];
        spans.extend(line.spans);
        out.push(Line {
            spans,
            style,
            alignment: line.alignment,
        });
    }
}

fn content_lines(content: &[ToolContent], theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for block in content {
        match block {
            ToolContent::Text { text } => {
                for line in clean_text(text).lines() {
                    lines.push(Line::from(Span::styled(
                        line.to_string(),
                        Style::default().fg(theme.fg()),
                    )));
                }
            }
            ToolContent::Diff {
                path,
                old_text,
                new_text,
            } => {
                lines.push(Line::from(Span::styled(
                    format!("diff {}", clean_line(path)),
                    Style::default()
                        .fg(theme.accent())
                        .add_modifier(Modifier::BOLD),
                )));
                for line in old_text.as_deref().unwrap_or_default().lines() {
                    lines.push(Line::from(Span::styled(
                        format!("-{}", clean_line(line)),
                        Style::default().fg(theme.danger()),
                    )));
                }
                for line in new_text.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("+{}", clean_line(line)),
                        Style::default().fg(theme.success()),
                    )));
                }
            }
            ToolContent::Terminal { terminal_id } => {
                lines.push(Line::from(Span::styled(
                    format!("terminal {}", clean_line(terminal_id)),
                    Style::default().fg(theme.muted()),
                )));
            }
            ToolContent::Image { mime_type, uri } => {
                lines.push(Line::from(Span::styled(
                    format!(
                        "image {}{}",
                        clean_line(mime_type),
                        uri.as_deref()
                            .map(|uri| format!(" <{}>", clean_line(uri)))
                            .unwrap_or_default()
                    ),
                    Style::default().fg(theme.muted()),
                )));
            }
            ToolContent::Audio { mime_type } => {
                lines.push(Line::from(Span::styled(
                    format!("audio {}", clean_line(mime_type)),
                    Style::default().fg(theme.muted()),
                )));
            }
            ToolContent::ResourceLink {
                uri,
                name,
                mime_type,
                title,
                description,
                size,
            } => {
                let label = clean_line(title.as_deref().unwrap_or(name));
                let mut detail = format!("link {label} <{}>", clean_line(uri));
                if let Some(mime) = mime_type {
                    detail.push_str(&format!(" {}", clean_line(mime)));
                }
                if let Some(size) = size {
                    detail.push_str(&format!(" {size} bytes"));
                }
                lines.push(Line::from(Span::styled(
                    detail,
                    Style::default().fg(theme.fg()),
                )));
                if let Some(description) = description {
                    lines.push(Line::from(Span::styled(
                        clean_line(description),
                        Style::default().fg(theme.muted()),
                    )));
                }
            }
            ToolContent::Resource {
                uri,
                mime_type,
                text,
                binary,
            } => {
                let kind = clean_line(mime_type.as_deref().unwrap_or("resource"));
                lines.push(Line::from(Span::styled(
                    format!(
                        "{kind} <{}>{}",
                        clean_line(uri),
                        if *binary { " (binary)" } else { "" }
                    ),
                    Style::default().fg(theme.fg()),
                )));
                if let Some(text) = text {
                    for line in clean_text(text).lines() {
                        lines.push(Line::from(Span::styled(
                            line.to_string(),
                            Style::default().fg(theme.fg()),
                        )));
                    }
                }
            }
        }
    }
    lines
}

fn push_error(out: &mut Vec<Line<'static>>, message: &str, theme: &Theme) {
    out.push(Line::from(Span::styled(
        "Error",
        Style::default()
            .fg(theme.danger())
            .add_modifier(Modifier::BOLD),
    )));
    for line in message.lines() {
        out.push(Line::from(Span::styled(
            line.to_string(),
            Style::default().fg(theme.danger()),
        )));
    }
}

fn push_boundary(out: &mut Vec<Line<'static>>, outcome: IdleOutcome, theme: &Theme) {
    let label = match outcome {
        IdleOutcome::Succeeded => "Turn finished",
        IdleOutcome::Failed => "Turn failed",
        IdleOutcome::Interrupted => "Turn interrupted",
    };
    out.push(Line::from(Span::styled(
        format!("— {label} —"),
        Style::default().fg(theme.muted()),
    )));
}

fn format_location(location: &ToolLocation) -> String {
    let path = clean_line(&location.path);
    match location.line {
        Some(line) => format!("{path}:{line}"),
        None => path,
    }
}

/// A one-line summary of a JSON value: common file/command keys first, then a
/// compact literal. Newlines are flattened so a collapsed row stays one line.
fn input_summary(value: &serde_json::Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        let text = one_line(text);
        return (!text.is_empty()).then_some(text);
    }
    if let Some(object) = value.as_object() {
        for key in [
            "command",
            "description",
            "path",
            "query",
            "pattern",
            "url",
            "name",
            "title",
        ] {
            if let Some(text) = object.get(key).and_then(|v| v.as_str()) {
                if !text.trim().is_empty() {
                    return Some(one_line(text));
                }
            }
        }
        if !object.is_empty() {
            return Some(one_line(&serde_json::to_string(value).ok()?));
        }
    }
    None
}

fn content_summary(content: &ToolContent) -> Option<String> {
    match content {
        ToolContent::Text { text } => text.lines().next().map(one_line),
        ToolContent::Diff { path, .. } => Some(format!("diff {}", clean_line(path))),
        ToolContent::Terminal { terminal_id } => {
            Some(format!("terminal {}", clean_line(terminal_id)))
        }
        ToolContent::Image { mime_type, .. } => Some(format!("image {}", clean_line(mime_type))),
        ToolContent::Audio { mime_type } => Some(format!("audio {}", clean_line(mime_type))),
        ToolContent::ResourceLink { name, uri, .. } => {
            Some(format!("{} <{}>", clean_line(name), clean_line(uri)))
        }
        ToolContent::Resource { uri, .. } => Some(format!("resource <{}>", clean_line(uri))),
    }
}

fn one_line(text: &str) -> String {
    let text = clean_line(text);
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = text.chars();
    let short: String = chars.by_ref().take(80).collect();
    if chars.next().is_some() {
        format!("{short}…")
    } else {
        short
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool_detail(
        name: &str,
        kind: ToolKind,
        status: ToolCallStatus,
        input: serde_json::Value,
    ) -> ToolCallDetail {
        ToolCallDetail {
            name: Some(name.to_string()),
            title: Some(format!("{name} title")),
            kind: Some(kind),
            status: Some(status),
            input: Some(input),
            ..Default::default()
        }
    }

    #[test]
    fn adjacent_text_deltas_coalesce_per_role() {
        let mut feed = AgentFeed::new();
        feed.apply(&AgentStateEvent::TextDelta {
            role: MessageRole::Assistant,
            text: "Hello, ".to_string(),
        });
        feed.apply(&AgentStateEvent::TextDelta {
            role: MessageRole::Assistant,
            text: "world".to_string(),
        });
        assert_eq!(feed.len(), 1);
        assert!(matches!(
            &feed.items().next().unwrap().kind,
            FeedKind::Message { role: MessageRole::Assistant, text } if text == "Hello, world"
        ));

        feed.apply(&AgentStateEvent::TextDelta {
            role: MessageRole::User,
            text: "Hi".to_string(),
        });
        assert_eq!(feed.len(), 2, "a role change starts a distinct message");
    }

    #[test]
    fn thoughts_coalesce_and_stop_streaming_when_a_message_follows() {
        let mut feed = AgentFeed::new();
        feed.apply(&AgentStateEvent::ReasoningDelta {
            text: "first ".to_string(),
        });
        feed.apply(&AgentStateEvent::ReasoningDelta {
            text: "second".to_string(),
        });
        assert_eq!(feed.len(), 1);
        let item = feed.items().next().unwrap();
        assert!(matches!(
            &item.kind,
            FeedKind::Thinking { text, streaming: true, expanded: false }
                if text == "first second"
        ));

        feed.apply(&AgentStateEvent::TextDelta {
            role: MessageRole::Assistant,
            text: "answer".to_string(),
        });
        let items: Vec<_> = feed.items().collect();
        assert_eq!(items.len(), 2);
        assert!(matches!(
            &items[0].kind,
            FeedKind::Thinking {
                streaming: false,
                ..
            }
        ));
    }

    #[test]
    fn non_text_message_blocks_render_in_order_as_placeholders() {
        let mut feed = AgentFeed::new();
        feed.apply(&AgentStateEvent::TextDelta {
            role: MessageRole::Assistant,
            text: "before".to_string(),
        });
        feed.apply(&AgentStateEvent::ContentBlock {
            role: ContentRole::Assistant,
            content: MessageContent::Image {
                mime_type: "image/png".to_string(),
                uri: Some("https://example.com/a.png".to_string()),
            },
        });
        feed.apply(&AgentStateEvent::ContentBlock {
            role: ContentRole::Thought,
            content: MessageContent::Resource {
                uri: "file:///tmp/a.bin".to_string(),
                mime_type: None,
                binary: true,
            },
        });
        feed.apply(&AgentStateEvent::TextDelta {
            role: MessageRole::Assistant,
            text: "after".to_string(),
        });

        assert_eq!(feed.len(), 4, "a content block splits the text run");
        let text = rendered_text(&feed);
        let before = text.find("before").expect("first text");
        let image = text
            .find("image image/png <https://example.com/a.png>")
            .expect("image placeholder");
        let resource = text
            .find("resource <file:///tmp/a.bin> (binary)")
            .expect("resource placeholder");
        let after = text.find("after").expect("last text");
        assert!(before < image && image < resource && resource < after);
        assert!(text.contains("Thinking"));
        assert_eq!(
            text.matches("https://example.com/a.png").count(),
            1,
            "the URI is shown as text and never auto-fetched or duplicated"
        );
    }

    #[test]
    fn tool_lifecycle_updates_one_card_by_id() {
        let mut feed = AgentFeed::new();
        feed.apply(&AgentStateEvent::ToolStarted {
            id: "call-1".to_string(),
            name: "read".to_string(),
            input: json!({ "path": "src/lib.rs" }),
            detail: Some(Box::new(tool_detail(
                "read",
                ToolKind::Read,
                ToolCallStatus::Pending,
                json!({ "path": "src/lib.rs" }),
            ))),
        });
        feed.apply(&AgentStateEvent::ToolUpdated {
            id: "call-1".to_string(),
            partial: json!({}),
            detail: Some(Box::new(tool_detail(
                "read",
                ToolKind::Read,
                ToolCallStatus::InProgress,
                json!({ "path": "src/lib.rs" }),
            ))),
        });
        feed.apply(&AgentStateEvent::ToolFinished {
            id: "call-1".to_string(),
            name: "read".to_string(),
            ok: true,
            output: Some(json!("contents")),
            detail: None,
        });

        assert_eq!(
            feed.len(),
            1,
            "lifecycle events must not duplicate the card"
        );
        let FeedKind::Tool(tool) = &feed.items().next().unwrap().kind else {
            panic!("expected a tool card");
        };
        assert_eq!(tool.id, "call-1");
        assert_eq!(tool.status, ToolCallStatus::Completed);
        assert_eq!(tool.kind, ToolKind::Read);
        assert_eq!(tool.input, Some(json!({ "path": "src/lib.rs" })));
        assert_eq!(tool.output, Some(json!("contents")));
        assert!(!tool.expanded, "updates keep the user's fold state");
    }

    #[test]
    fn a_late_start_does_not_downgrade_a_running_card() {
        let mut feed = AgentFeed::new();
        feed.apply(&AgentStateEvent::ToolUpdated {
            id: "t".to_string(),
            partial: json!({}),
            detail: Some(Box::new(ToolCallDetail {
                status: Some(ToolCallStatus::InProgress),
                ..Default::default()
            })),
        });
        feed.apply(&AgentStateEvent::ToolStarted {
            id: "t".to_string(),
            name: "read".to_string(),
            input: json!({}),
            detail: Some(Box::new(ToolCallDetail {
                status: Some(ToolCallStatus::Pending),
                ..Default::default()
            })),
        });
        let FeedKind::Tool(tool) = &feed.items().next().unwrap().kind else {
            panic!("expected a tool card");
        };
        assert_eq!(tool.status, ToolCallStatus::InProgress);
    }

    #[test]
    fn a_snapshot_with_empty_content_clears_previous_content_and_locations() {
        let mut feed = AgentFeed::new();
        feed.apply(&AgentStateEvent::ToolStarted {
            id: "t".to_string(),
            name: "read".to_string(),
            input: json!({}),
            detail: Some(Box::new(ToolCallDetail {
                status: Some(ToolCallStatus::InProgress),
                content: vec![ToolContent::Text {
                    text: "stale".to_string(),
                }],
                locations: vec![ToolLocation {
                    path: "stale.rs".to_string(),
                    line: None,
                }],
                ..Default::default()
            })),
        });

        feed.apply(&AgentStateEvent::ToolUpdated {
            id: "t".to_string(),
            partial: json!({}),
            detail: Some(Box::new(ToolCallDetail {
                status: Some(ToolCallStatus::InProgress),
                ..Default::default()
            })),
        });

        let FeedKind::Tool(tool) = &feed.items().next().unwrap().kind else {
            panic!("expected a tool card");
        };
        assert!(tool.content.is_empty(), "empty snapshot must clear content");
        assert!(
            tool.locations.is_empty(),
            "empty snapshot must clear locations"
        );
    }

    #[test]
    fn interleaved_updates_preserve_order_without_duplicates() {
        let mut feed = AgentFeed::new();
        feed.apply(&AgentStateEvent::ToolStarted {
            id: "a".to_string(),
            name: "read".to_string(),
            input: json!({}),
            detail: None,
        });
        feed.apply(&AgentStateEvent::TextDelta {
            role: MessageRole::Assistant,
            text: "between".to_string(),
        });
        feed.apply(&AgentStateEvent::ToolUpdated {
            id: "a".to_string(),
            partial: json!("progress"),
            detail: None,
        });
        feed.apply(&AgentStateEvent::ToolStarted {
            id: "b".to_string(),
            name: "bash".to_string(),
            input: json!({}),
            detail: None,
        });

        let kinds: Vec<_> = feed
            .items()
            .map(|item| match &item.kind {
                FeedKind::Tool(tool) => format!("tool:{}", tool.id),
                FeedKind::Message { .. } => "message".to_string(),
                _ => "other".to_string(),
            })
            .collect();
        assert_eq!(kinds, ["tool:a", "message", "tool:b"]);
        assert_eq!(feed.tool_ids(), ["a", "b"]);
    }

    #[test]
    fn tool_update_before_start_creates_a_card_that_the_start_merges_into() {
        let mut feed = AgentFeed::new();
        feed.apply(&AgentStateEvent::ToolUpdated {
            id: "early".to_string(),
            partial: json!({ "status": "working" }),
            detail: None,
        });
        feed.apply(&AgentStateEvent::ToolStarted {
            id: "early".to_string(),
            name: "bash".to_string(),
            input: json!({ "command": "ls" }),
            detail: None,
        });
        assert_eq!(feed.len(), 1);
        let FeedKind::Tool(tool) = &feed.items().next().unwrap().kind else {
            panic!("expected a tool card");
        };
        assert_eq!(tool.name, "bash");
        assert_eq!(tool.input, Some(json!({ "command": "ls" })));
    }

    #[test]
    fn folding_a_tool_preserves_the_state_across_updates() {
        let mut feed = AgentFeed::new();
        feed.apply(&AgentStateEvent::ToolStarted {
            id: "t".to_string(),
            name: "bash".to_string(),
            input: json!({ "command": "ls" }),
            detail: None,
        });
        let item = feed.items().next().unwrap().id;
        assert!(feed.toggle_item(item));
        assert_eq!(feed.tool_expanded("t"), Some(true));
        feed.apply(&AgentStateEvent::ToolFinished {
            id: "t".to_string(),
            name: "bash".to_string(),
            ok: true,
            output: Some(json!("ok")),
            detail: None,
        });
        assert_eq!(feed.tool_expanded("t"), Some(true));
        assert!(feed.toggle_item(item));
        assert_eq!(feed.tool_expanded("t"), Some(false));
    }

    #[test]
    fn unknown_kind_and_status_have_neutral_fallbacks() {
        let mut feed = AgentFeed::new();
        feed.apply(&AgentStateEvent::ToolStarted {
            id: "u".to_string(),
            name: "mystery".to_string(),
            input: json!({}),
            detail: Some(Box::new(ToolCallDetail {
                kind: Some(ToolKind::Other),
                status: Some(ToolCallStatus::Unknown),
                ..Default::default()
            })),
        });
        let FeedKind::Tool(tool) = &feed.items().next().unwrap().kind else {
            panic!("expected a tool card");
        };
        assert_eq!(tool.kind_icon(), "•");
        assert_eq!(tool.status_label(), "unknown");
    }

    #[test]
    fn feed_is_bounded_and_drops_old_tool_cards() {
        let mut feed = AgentFeed::new();
        for i in 0..(MAX_FEED_ITEMS + 3) {
            feed.apply(&AgentStateEvent::ToolStarted {
                id: format!("t{i}"),
                name: "bash".to_string(),
                input: json!({}),
                detail: None,
            });
        }
        assert_eq!(feed.len(), MAX_FEED_ITEMS);
        assert!(!feed.tool_ids().contains(&"t0"), "oldest card was pruned");
        assert!(feed
            .tool_ids()
            .contains(&format!("t{}", MAX_FEED_ITEMS + 2).as_str()));
    }

    #[test]
    fn control_sequences_are_stripped_from_agent_text() {
        let mut feed = AgentFeed::new();
        feed.apply(&AgentStateEvent::TextDelta {
            role: MessageRole::Assistant,
            text: "\x1b[31mred\x1b[0m and \x1b]0;title\x07plain".to_string(),
        });
        let FeedKind::Message { text, .. } = &feed.items().next().unwrap().kind else {
            panic!("expected a message");
        };
        assert_eq!(text, "red and plain");
    }

    fn rendered_text_with(feed: &AgentFeed, theme: &Theme) -> String {
        feed.render(theme, None)
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn rendered_text(feed: &AgentFeed) -> String {
        rendered_text_with(feed, &Theme::dark())
    }

    #[test]
    fn tool_metadata_paths_and_content_cannot_inject_controls() {
        let mut feed = AgentFeed::new();
        feed.apply(&AgentStateEvent::ToolStarted {
            id: "escape".to_string(),
            name: "read\x1b[2J".to_string(),
            input: json!({ "command": "echo \u{202e}evil", "path": "src/\u{200b}lib.rs" }),
            detail: Some(Box::new(ToolCallDetail {
                title: Some("Ti\x1b]0;pwned\x07tle".to_string()),
                kind: Some(ToolKind::Edit),
                status: Some(ToolCallStatus::Completed),
                locations: vec![ToolLocation {
                    path: "/tmp/\u{202e}etc".to_string(),
                    line: Some(3),
                }],
                content: vec![
                    ToolContent::Text {
                        text: "body\x1b[31m".to_string(),
                    },
                    ToolContent::Diff {
                        path: "p\u{202e}".to_string(),
                        old_text: Some("a\x1b[0m".to_string()),
                        new_text: "b\u{202e}".to_string(),
                    },
                ],
                ..Default::default()
            })),
        });
        let item = feed.items().next().unwrap().id;
        assert!(feed.toggle_item(item));

        let text = rendered_text(&feed);
        assert!(!text.contains('\x1b'), "escape leaked: {text:?}");
        assert!(!text.contains('\u{202e}'), "bidi override leaked: {text:?}");
        assert!(!text.contains('\u{200b}'), "zero-width leaked: {text:?}");
        assert!(text.contains("[U+202E]"), "bidi not made visible: {text:?}");
        assert!(text.contains("read"), "tool name missing: {text:?}");
        assert!(text.contains("Title"), "tool title missing: {text:?}");
        assert!(
            text.contains("/tmp/[U+202E]etc:3"),
            "location not shown safely: {text:?}"
        );
        assert!(text.contains("body"), "text content missing: {text:?}");
    }

    #[test]
    fn assistant_links_are_inert_and_keep_a_distinct_target() {
        let mut feed = AgentFeed::new();
        feed.apply(&AgentStateEvent::TextDelta {
            role: MessageRole::Assistant,
            text: "[docs](https://example.com/a?b=c) and [https://same.example/path](https://same.example/path) and <b>not html</b>"
                .to_string(),
        });
        let text = rendered_text(&feed);
        assert!(
            text.contains("docs (https://example.com/a?b=c)"),
            "distinct target missing: {text:?}"
        );
        assert_eq!(
            text.matches("https://same.example/path").count(),
            1,
            "a target equal to its label should not be repeated: {text:?}"
        );
        assert!(
            text.contains("<b>not html</b>"),
            "raw HTML must stay literal: {text:?}"
        );
    }

    #[test]
    fn selected_thought_is_visible_without_colour() {
        let mut feed = AgentFeed::new();
        feed.apply(&AgentStateEvent::ReasoningDelta {
            text: "a private thought".to_string(),
        });
        let item = feed.items().next().unwrap().id;
        let rendered = feed.render(&Theme::monochrome(), Some(item));
        assert_eq!(rendered.selected_range, Some((0, 1)));
        assert!(
            rendered.lines[0]
                .style
                .add_modifier
                .contains(Modifier::REVERSED),
            "selection must not rely on colour: {:?}",
            rendered.lines[0].style
        );
        assert!(!rendered_text(&feed).contains("a private thought"));
    }

    #[test]
    fn status_labels_remain_readable_without_colour() {
        let mut feed = AgentFeed::new();
        feed.apply(&AgentStateEvent::ToolFinished {
            id: "t".to_string(),
            name: "bash".to_string(),
            ok: false,
            output: None,
            detail: None,
        });
        let text = rendered_text_with(&feed, &Theme::monochrome());
        assert!(text.contains("bash"), "tool name missing: {text:?}");
        assert!(text.contains("failed"), "status label missing: {text:?}");
    }

    #[test]
    fn rendering_marks_thoughts_and_tool_status_textually() {
        let mut feed = AgentFeed::new();
        feed.apply(&AgentStateEvent::ReasoningDelta {
            text: "a thought".to_string(),
        });
        let streaming = rendered_text(&feed);
        assert!(streaming.contains("Thinking"));
        assert!(streaming.contains("streaming…"));
        assert!(
            !streaming.contains("a thought"),
            "thought body starts folded"
        );

        feed.apply(&AgentStateEvent::ToolFinished {
            id: "t".to_string(),
            name: "bash".to_string(),
            ok: false,
            output: None,
            detail: None,
        });
        let finished = rendered_text(&feed);
        assert!(finished.contains("failed"));
        assert!(
            !finished.contains("streaming…"),
            "a trailing thought stops streaming once another event follows"
        );
    }

    #[test]
    fn assistant_markdown_renders_headings_links_and_fenced_code() {
        let mut feed = AgentFeed::new();
        feed.apply(&AgentStateEvent::TextDelta {
            role: MessageRole::Assistant,
            text: "# Heading\n\nA [link](https://example.com).\n\n```rust\nfn main() {}\n```"
                .to_string(),
        });
        let theme = Theme::dark();
        let rendered = feed.render(&theme, None);
        let text = rendered
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("# Heading"));
        assert!(text.contains("link (https://example.com)"));
        assert!(text.contains("```rust"));
        assert!(text.contains("fn main() {}"));

        let spans: Vec<_> = rendered
            .lines
            .into_iter()
            .flat_map(|line| line.spans)
            .collect();
        assert!(spans
            .iter()
            .any(|s| s.content == "# Heading" && s.style.fg == Some(theme.syn_heading())));
        assert!(spans
            .iter()
            .any(|s| s.content == "link" && s.style.add_modifier.contains(Modifier::UNDERLINED)));
        assert!(spans
            .iter()
            .any(|s| s.content == "fn" && s.style.fg == Some(theme.syntax.keyword)));
    }

    #[test]
    fn user_messages_and_tool_fields_are_not_rendered_as_markdown() {
        let mut feed = AgentFeed::new();
        feed.apply(&AgentStateEvent::TextDelta {
            role: MessageRole::User,
            text: "# not a heading".to_string(),
        });
        feed.apply(&AgentStateEvent::ToolStarted {
            id: "t".to_string(),
            name: "bash".to_string(),
            input: json!({ "command": "echo **x**" }),
            detail: Some(Box::new(ToolCallDetail {
                title: Some("**tool title**".to_string()),
                ..Default::default()
            })),
        });

        let theme = Theme::dark();
        let rendered = feed.render(&theme, None);
        let text = rendered
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("# not a heading"));
        assert!(
            text.contains("**tool title**"),
            "tool titles must stay literal: {text:?}"
        );
        let user = rendered
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content == "# not a heading")
            .expect("user text is rendered");
        assert!(
            user.style.add_modifier.is_empty(),
            "user text must stay plain: {:?}",
            user.style
        );
    }

    #[test]
    fn expanding_a_tool_reveals_input_output_locations_and_diff_content() {
        let mut feed = AgentFeed::new();
        feed.apply(&AgentStateEvent::ToolStarted {
            id: "t".to_string(),
            name: "edit".to_string(),
            input: json!({ "path": "src/lib.rs" }),
            detail: Some(Box::new(ToolCallDetail {
                kind: Some(ToolKind::Edit),
                status: Some(ToolCallStatus::Completed),
                input: Some(json!({ "path": "src/lib.rs" })),
                output: Some(json!({ "changed": true })),
                locations: vec![ToolLocation {
                    path: "src/lib.rs".to_string(),
                    line: Some(4),
                }],
                content: vec![
                    ToolContent::Diff {
                        path: "src/lib.rs".to_string(),
                        old_text: Some("old".to_string()),
                        new_text: "new".to_string(),
                    },
                    ToolContent::Terminal {
                        terminal_id: "term-1".to_string(),
                    },
                ],
                ..Default::default()
            })),
        });

        let folded = rendered_text(&feed);
        assert!(folded.contains("edit"));
        assert!(folded.contains("completed"));
        assert!(!folded.contains("src/lib.rs:4"));
        assert!(!folded.contains("+new"));

        let item = feed.items().next().unwrap().id;
        assert!(feed.toggle_item(item));
        let expanded = rendered_text(&feed);
        assert!(expanded.contains("Input"));
        assert!(expanded.contains("Output"));
        assert!(expanded.contains("Locations"));
        assert!(expanded.contains("src/lib.rs:4"));
        assert!(expanded.contains("diff src/lib.rs"));
        assert!(expanded.contains("-old"));
        assert!(expanded.contains("+new"));
        assert!(expanded.contains("terminal term-1"));
    }
}
