//! The Inbox: the default landing view.
//!
//! It is a UI-only hub for everything that needs a human: the tasks blocked in
//! `AwaitingInput` (newest first) with their agent prompt and options, the most
//! recent failures, and the last 20 persisted events. The task/event data comes
//! from the shared [`AppState`] cache, seeded here with `tasks.list` /
//! `agents.list` / `events.tail` and kept live by the existing SSE reader, so a
//! `task.updated` or `event` push re-renders the page with no extra wiring.
//!
//! Answering a prompt (`agents.reply`) is wired here: the option buttons, the
//! "Always" control, and the free-text input map to a typed
//! [`InputReply`](favetto_wire::model::InputReply) with the same kind-aware
//! rules as the TUI, and a successful reply re-seeds the cache so the card
//! disappears immediately (an SSE `task.updated` push does the same once the
//! daemon resolves the request).

use dioxus::prelude::*;
use favetto_wire::model::{
    AgentCatalogEntry, AwaitingInputKind, Event, InputReply, Task, TaskStatus,
};
use favetto_wire::rpc::{AgentReplyParams, EmptyParams, EventsTailParams, TasksListParams};
use uuid::Uuid;

use crate::app::Route;
use crate::client::RpcClient;
use crate::components::common::{EmptyState, EventBadge, Spinner};
use crate::components::task_card::TaskCard;
use crate::components::toast::{Toast, ToastKind};
use crate::config;
use crate::state::{onboarding_step, task_display_name, task_repo, AppState, OnboardingStep};
use crate::views::catalog::{kind_label, task_tags};
use crate::views::tasks::{format_age, now_ms, status_label, KindBadge};

/// How many recent failures the Inbox shows.
pub const FAILURES_LIMIT: usize = 5;
/// How many events the live feed shows.
pub const EVENTS_LIMIT: usize = 20;
/// Drag distance (px) that triggers a pull-to-refresh.
pub const PULL_THRESHOLD: f64 = 70.0;

/// Copy shown when a reply is attempted on a screen-detected prompt, which has
/// no structured input channel to answer through.
pub const SCREEN_DETECTED_REPLY: &str =
    "This prompt was detected from the terminal; answer it from the agent's terminal.";

/// Tasks in `AwaitingInput`, newest first.
pub fn awaiting_tasks(tasks: &[Task]) -> Vec<Task> {
    let mut rows: Vec<Task> = tasks
        .iter()
        .filter(|task| task.status == TaskStatus::AwaitingInput)
        .cloned()
        .collect();
    sort_newest_first(&mut rows);
    rows
}

/// The most recent failures, newest first, capped at `limit`.
pub fn recent_failures(tasks: &[Task], limit: usize) -> Vec<Task> {
    let mut rows: Vec<Task> = tasks
        .iter()
        .filter(|task| task.status == TaskStatus::Failed)
        .cloned()
        .collect();
    sort_newest_first(&mut rows);
    rows.truncate(limit);
    rows
}

/// The most recent events, newest first, capped at `limit`.
///
/// The cache stores events ascending by id, so this reverses and truncates.
pub fn recent_events(events: &[Event], limit: usize) -> Vec<Event> {
    let mut rows: Vec<Event> = events.to_vec();
    rows.sort_by_key(|event| std::cmp::Reverse(event.id));
    rows.truncate(limit);
    rows
}

/// Sort tasks by creation time, then id, descending (newest first).
fn sort_newest_first(rows: &mut [Task]) {
    rows.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.id.cmp(&a.id))
    });
}

/// The prompt an awaiting task's agent session is blocked on.
///
/// Built from the `agents.list` `AwaitingInputReason` (see [`AgentSessionInfo`]);
/// `request_id` is `None` for screen-detected prompts, which only support a
/// free-text answer.
///
/// [`AgentSessionInfo`]: favetto_wire::model::AgentSessionInfo
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AwaitingCard {
    /// The `agents.reply` session id.
    pub session_id: String,
    /// The correlating request id, absent for screen-detected prompts.
    pub request_id: Option<String>,
    /// What kind of decision the agent is blocked on; drives the reply mapping.
    pub kind: AwaitingInputKind,
    /// The prompt text.
    pub message: String,
    /// Selectable options; empty means free-form text.
    pub options: Vec<String>,
    /// Whether an "always" answer is offered.
    pub allow_always: bool,
}

/// A reply the user chose in the Inbox, before it is mapped to the wire type.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ReplyIntent {
    /// The option at this index into [`AwaitingCard::options`].
    Option(usize),
    /// The extra "always / remember" control.
    Always,
    /// A free-form answer.
    Text(String),
}

impl AwaitingCard {
    /// Map a UI [`ReplyIntent`] to the typed [`InputReply`] the daemon expects.
    pub fn to_reply(&self, intent: &ReplyIntent) -> InputReply {
        match intent {
            ReplyIntent::Always => InputReply::Always,
            ReplyIntent::Text(text) => InputReply::Value {
                value: text.clone(),
            },
            ReplyIntent::Option(index) => {
                input_reply_for(self.kind, &self.options, self.allow_always, *index, "")
            }
        }
    }
}

/// Map a selected option (or free-form value) to a typed [`InputReply`].
///
/// The mapping is kind-aware so a permission prompt answers with
/// `Once`/`Always`/`Reject`, a confirmation with `Confirmed`, and everything
/// else (choices, free-form text) with `Value`. This mirrors the TUI's
/// `reply_for` so both clients answer a prompt identically.
pub fn input_reply_for(
    kind: AwaitingInputKind,
    options: &[String],
    allow_always: bool,
    selected: usize,
    value: &str,
) -> InputReply {
    if options.is_empty() {
        return InputReply::Value {
            value: value.to_string(),
        };
    }
    let label = options.get(selected).cloned().unwrap_or_default();
    let lower = label.to_ascii_lowercase();
    match kind {
        AwaitingInputKind::Permission => {
            if lower.contains("always") || lower.contains("remember") {
                InputReply::Always
            } else if lower.contains("reject")
                || lower.contains("deny")
                || lower.contains("block")
                || lower.contains("cancel")
            {
                InputReply::Reject
            } else if allow_always && selected == 1 && options.len() > 2 {
                InputReply::Always
            } else {
                InputReply::Once
            }
        }
        AwaitingInputKind::Confirmation => {
            let confirmed = !(lower.contains("no")
                || lower.contains("deny")
                || lower.contains("reject")
                || lower.contains("cancel"));
            InputReply::Confirmed { confirmed }
        }
        AwaitingInputKind::Choice | AwaitingInputKind::Pinentry | AwaitingInputKind::Other => {
            InputReply::Value { value: label }
        }
    }
}

/// Find the structured awaiting reason for `task` among the live agent sessions.
///
/// Returns `None` when no session matches or the prompt was only detected on
/// screen (there is still an [`AwaitingCard`] in that case, with
/// `request_id: None`).
pub fn awaiting_card_for(task: &Task, agents: &[AgentCatalogEntry]) -> Option<AwaitingCard> {
    let task_id = task.id.to_string();
    agents
        .iter()
        .flat_map(|agent| agent.sessions.iter())
        .find(|session| {
            session.task_id.as_deref() == Some(task_id.as_str()) && session.awaiting_input.is_some()
        })
        .and_then(|session| {
            session.awaiting_input.as_ref().map(|reason| AwaitingCard {
                session_id: session.id.clone(),
                request_id: reason.request_id.clone(),
                kind: reason.kind,
                message: reason.message.clone(),
                options: reason.options.clone(),
                allow_always: reason.allow_always,
            })
        })
}

/// Clamp the pull distance to be non-negative.
pub fn pull_distance(start_y: f64, end_y: f64) -> f64 {
    (end_y - start_y).max(0.0)
}

/// Whether a pull gesture crossed the refresh threshold.
pub fn pull_refresh_reached(start_y: f64, end_y: f64, threshold: f64) -> bool {
    pull_distance(start_y, end_y) >= threshold
}

/// The `/` (and `/inbox`) default view.
#[component]
pub fn Inbox() -> Element {
    let mut state = use_context::<AppState>();
    let nav = use_navigator();

    // Guards the authenticated area: until the client is `Ready` it redirects to
    // `/pair` (the daemon-URL step on Android, the pairing step otherwise).
    use_effect(move || {
        let step = onboarding_step(
            config::daemon_url_required(),
            &(state.daemon_url)(),
            (state.token)().as_deref(),
        );
        if step != OnboardingStep::Ready {
            nav.replace(Route::Pair {});
        }
    });

    let mut agents = use_signal(Vec::<AgentCatalogEntry>::new);
    // Bumped by the Refresh button, the pull-to-refresh gesture, and a
    // successful reply; the resource below subscribes to it and re-runs.
    let mut refresh = use_signal(|| 0u64);
    // The current toast, if any.
    let mut toast = use_signal(|| Option::<(ToastKind, String)>::None);
    // True while a reply is in flight, so every reply control is disabled.
    let busy = use_signal(|| false);
    let mut pull_start = use_signal(|| Option::<f64>::None);

    // Seed the caches. `daemon_url`/`token` are read in the synchronous part so
    // the resource subscribes to them too.
    let load = use_resource(move || {
        let base = (state.daemon_url)();
        let token = (state.token)();
        let _refresh = refresh();
        async move {
            let client = RpcClient::new(base).with_token(token);
            let tasks = client
                .tasks_list(TasksListParams { limit: Some(500) })
                .await
                .map_err(|err| err.to_string())?;
            state.cache.write().set_tasks(tasks);

            // The catalog powers the task cards' metadata chips. A failing
            // catalog is non-fatal: cards fall back to the repo chip.
            if let Ok(catalog) = client.catalog_list(EmptyParams {}).await {
                state.cache.write().set_catalog(catalog);
            }

            let list = client
                .agents_list(EmptyParams {})
                .await
                .map_err(|err| err.to_string())?;
            agents.set(list);

            let events = client
                .events_tail(EventsTailParams {
                    limit: Some(EVENTS_LIMIT as u64),
                })
                .await
                .map_err(|err| err.to_string())?;
            {
                let mut cache = state.cache.write();
                for event in events {
                    cache.push_event(event);
                }
            }
            Ok::<(), String>(())
        }
    });

    let now = now_ms();
    let cache = (state.cache)();
    // Scope the whole Inbox to the active repo (all rows when global).
    let tasks: Vec<Task> = cache.visible_tasks().into_iter().cloned().collect();
    let awaiting = awaiting_tasks(&tasks);
    let failures = recent_failures(&tasks, FAILURES_LIMIT);
    let visible_events: Vec<Event> = cache.visible_events().into_iter().cloned().collect();
    let events = recent_events(&visible_events, EVENTS_LIMIT);
    let agent_list = agents();

    let awaiting_rows: Vec<(Task, Option<AwaitingCard>)> = awaiting
        .into_iter()
        .map(|task| {
            let card = awaiting_card_for(&task, &agent_list);
            (task, card)
        })
        .collect();

    rsx! {
        main {
            id: "favetto-app",
            class: "inbox-view",
            ontouchstart: move |event| {
                pull_start.set(event.touches().first().map(|touch| touch.client_coordinates().y));
            },
            ontouchend: move |event| {
                let changed = event.touches_changed();
                let active = event.touches();
                let end = changed
                    .first()
                    .or_else(|| active.first())
                    .map(|touch| touch.client_coordinates().y);
                if let (Some(start), Some(end)) = (pull_start(), end) {
                    if pull_refresh_reached(start, end, PULL_THRESHOLD) {
                        refresh.set(refresh() + 1);
                    }
                }
                pull_start.set(None);
            },
            header { class: "inbox-header",
                h1 { "Inbox" }
                button {
                    class: "btn inbox-refresh",
                    onclick: move |_| refresh.set(refresh() + 1),
                    "Refresh"
                }
            }
            if load().is_none() {
                Spinner {}
            }
            if let Some(Err(err)) = load() {
                div { class: "error-banner", role: "alert", "Failed to load the Inbox: {err}" }
            }
            if let Some((kind, message)) = toast() {
                Toast {
                    kind,
                    message,
                    on_dismiss: move |_| toast.set(None),
                }
            }
            section { class: "inbox-section awaiting-section",
                h2 { "Awaiting input" }
                if awaiting_rows.is_empty() {
                    EmptyState { message: "All caught up.".to_string() }
                } else {
                    for (task, card) in awaiting_rows {
                        if let Some(card) = card {
                            AwaitingInputCard {
                                key: "{task.id}",
                                id: task.id,
                                name: task_display_name(&task.name).to_string(),
                                status: task.status,
                                age: format_age(task.created_at.timestamp_millis(), now),
                                repo: task_repo(&task.name).map(str::to_string),
                                tags: cache.catalog_entry(&task.name).map(task_tags).unwrap_or_default(),
                                kind: cache.catalog_entry(&task.name).map(kind_label).map(str::to_string),
                                card,
                                busy,
                                toast,
                                refresh,
                            }
                        } else {
                            TaskCard {
                                key: "{task.id}",
                                id: task.id,
                                name: task_display_name(&task.name).to_string(),
                                status: task.status,
                                age: format_age(task.created_at.timestamp_millis(), now),
                                awaiting: true,
                                repo: task_repo(&task.name).map(str::to_string),
                                tags: cache.catalog_entry(&task.name).map(task_tags).unwrap_or_default(),
                                kind: cache.catalog_entry(&task.name).map(kind_label).map(str::to_string),
                            }
                        }
                    }
                }
            }
            if !failures.is_empty() {
                section { class: "inbox-section failures-section",
                    h2 { "Recent failures" }
                    for task in failures {
                        TaskCard {
                            key: "{task.id}",
                            id: task.id,
                            name: task_display_name(&task.name).to_string(),
                            status: task.status,
                            age: format_age(task.created_at.timestamp_millis(), now),
                            detail: task.error.clone(),
                            repo: task_repo(&task.name).map(str::to_string),
                            tags: cache.catalog_entry(&task.name).map(task_tags).unwrap_or_default(),
                            kind: cache.catalog_entry(&task.name).map(kind_label).map(str::to_string),
                        }
                    }
                }
            }
            section { class: "inbox-section events-section",
                h2 { "Recent events" }
                if events.is_empty() {
                    EmptyState { message: "No events yet.".to_string() }
                } else {
                    ul { class: "event-feed",
                        for event in events {
                            li { key: "{event.id}", class: "event-row",
                                EventBadge { kind: event.kind.clone() }
                                span {
                                    class: "event-time",
                                    title: "{event.created_at}",
                                    "{format_age(event.created_at.timestamp_millis(), now)}"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Send one `agents.reply` and fold the outcome into the toast and refresh
/// signals. A success re-seeds the cache so the resolved card disappears even
/// before the matching `task.updated` push arrives.
pub(crate) fn submit_reply(
    state: AppState,
    mut busy: Signal<bool>,
    mut toast: Signal<Option<(ToastKind, String)>>,
    mut refresh: Signal<u64>,
    session_id: String,
    request_id: String,
    reply: InputReply,
) {
    busy.set(true);
    spawn(async move {
        let client = RpcClient::new((state.daemon_url)()).with_token((state.token)());
        let params = AgentReplyParams {
            session_id,
            request_id,
            reply,
        };
        match client.agents_reply(params).await {
            Ok(_) => {
                toast.set(Some((ToastKind::Success, "Reply sent.".to_string())));
                refresh.set(refresh() + 1);
            }
            Err(err) => toast.set(Some((ToastKind::Error, err.to_string()))),
        }
        busy.set(false);
    });
}

/// One awaiting-input card: the agent prompt plus its options, an "always"
/// control, or a free-text fallback. A screen-detected prompt (no structured
/// request id) cannot be answered through `agents.reply`, so it only links to
/// the task and explains where to answer.
#[component]
fn AwaitingInputCard(
    id: Uuid,
    name: String,
    status: TaskStatus,
    age: String,
    #[props(default)] repo: Option<String>,
    #[props(default)] tags: Vec<String>,
    /// The catalog-derived kind label (`agent` / `action` / `scheduled`).
    #[props(default)]
    kind: Option<String>,
    card: AwaitingCard,
    busy: Signal<bool>,
    toast: Signal<Option<(ToastKind, String)>>,
    refresh: Signal<u64>,
) -> Element {
    let state = use_context::<AppState>();
    let mut draft = use_signal(String::new);
    let message = card.message.clone();
    let options = card.options.clone();
    let allow_always = card.allow_always;
    let screen_detected = card.request_id.is_none();
    let badge = format!("status-badge status-{}", status.as_str());

    // Stable handle so every control can submit without cloning the card.
    let submit = use_callback(move |intent: ReplyIntent| {
        let Some(request_id) = card.request_id.clone() else {
            let mut toast = toast;
            toast.set(Some((ToastKind::Error, SCREEN_DETECTED_REPLY.to_string())));
            return;
        };
        let reply = card.to_reply(&intent);
        let session_id = card.session_id.clone();
        submit_reply(state, busy, toast, refresh, session_id, request_id, reply);
    });

    rsx! {
        div { class: "awaiting-card",
            div { class: "awaiting-head",
                Link {
                    to: Route::TaskDetail { id: id.to_string() },
                    class: "awaiting-name",
                    "{name}"
                }
                span { class: badge, "{status_label(status)}" }
                if let Some(kind) = kind {
                    KindBadge { kind }
                }
                span { class: "awaiting-age", "{age}" }
                if let Some(repo) = repo {
                    span { class: "task-card-repo", "{repo}" }
                }
                for tag in tags {
                    span { class: "tag", "{tag}" }
                }
            }
            p { class: "awaiting-prompt", "{message}" }
            if screen_detected {
                p { class: "awaiting-hint",
                    "This prompt was detected from the terminal; answer it from the agent's terminal."
                }
            } else if options.is_empty() {
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
    use favetto_wire::model::{AwaitingInputKind, AwaitingInputReason};

    fn make_task(id: Uuid, name: &str, status: TaskStatus, created_at: &str) -> Task {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "name": name,
            "status": status.as_str(),
            "attempt": 0,
            "input": {},
            "output": null,
            "dedupe_key": null,
            "created_at": created_at,
            "started_at": null,
            "finished_at": null,
            "error": null,
            "failure": null,
            "session_id": null,
            "session_title": null,
            "parent_id": null,
            "root_id": null,
            "interactive": false,
        }))
        .unwrap()
    }

    fn event(id: i64) -> Event {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "kind": "task_finished",
            "payload": { "id": id },
            "created_at": "2026-01-01T00:00:00Z",
        }))
        .unwrap()
    }

    fn agent_with_session(
        session_id: &str,
        task_id: Option<&str>,
        awaiting: Option<AwaitingInputReason>,
    ) -> AgentCatalogEntry {
        serde_json::from_value(serde_json::json!({
            "name": "opencode",
            "display_name": "OpenCode",
            "command": "opencode",
            "default": true,
            "available": true,
            "capabilities": {},
            "sessions": [{
                "id": session_id,
                "agent": "opencode",
                "task_id": task_id,
                "running": true,
                "headless": false,
                "awaiting_input": awaiting,
            }],
        }))
        .unwrap()
    }

    fn reason(
        request_id: Option<&str>,
        options: Vec<&str>,
        allow_always: bool,
    ) -> AwaitingInputReason {
        AwaitingInputReason {
            kind: AwaitingInputKind::Permission,
            message: "Allow this command?".to_string(),
            request_id: request_id.map(str::to_string),
            options: options.into_iter().map(str::to_string).collect(),
            allow_always,
        }
    }

    fn card(kind: AwaitingInputKind, options: Vec<&str>, allow_always: bool) -> AwaitingCard {
        AwaitingCard {
            session_id: "sess".to_string(),
            request_id: Some("req".to_string()),
            kind,
            message: "?".to_string(),
            options: options.into_iter().map(str::to_string).collect(),
            allow_always,
        }
    }

    #[test]
    fn input_reply_for_maps_permission_options() {
        let options = vec![
            "Allow".to_string(),
            "Allow always".to_string(),
            "Deny".to_string(),
        ];
        assert_eq!(
            input_reply_for(AwaitingInputKind::Permission, &options, false, 0, ""),
            InputReply::Once
        );
        // A label mentioning "always"/"remember" wins over the index.
        assert_eq!(
            input_reply_for(AwaitingInputKind::Permission, &options, false, 1, ""),
            InputReply::Always
        );
        assert_eq!(
            input_reply_for(AwaitingInputKind::Permission, &options, false, 2, ""),
            InputReply::Reject
        );
        // `allow_always` promotes the middle option when there are 3+.
        let plain = vec![
            "Allow".to_string(),
            "Always".to_string(),
            "Deny".to_string(),
        ];
        assert_eq!(
            input_reply_for(AwaitingInputKind::Permission, &plain, true, 1, ""),
            InputReply::Always
        );
    }

    #[test]
    fn input_reply_for_maps_confirmation_choice_and_free_text() {
        let yes_no = vec!["Yes".to_string(), "No".to_string()];
        assert_eq!(
            input_reply_for(AwaitingInputKind::Confirmation, &yes_no, false, 0, ""),
            InputReply::Confirmed { confirmed: true }
        );
        assert_eq!(
            input_reply_for(AwaitingInputKind::Confirmation, &yes_no, false, 1, ""),
            InputReply::Confirmed { confirmed: false }
        );
        let choice = vec!["Alpha".to_string(), "Beta".to_string()];
        assert_eq!(
            input_reply_for(AwaitingInputKind::Choice, &choice, false, 1, ""),
            InputReply::Value {
                value: "Beta".to_string()
            }
        );
        // Empty options mean free text, regardless of kind.
        assert_eq!(
            input_reply_for(AwaitingInputKind::Other, &[], false, 0, "hello"),
            InputReply::Value {
                value: "hello".to_string()
            }
        );
    }

    #[test]
    fn to_reply_maps_every_intent() {
        let card = card(AwaitingInputKind::Permission, vec!["Allow", "Deny"], true);
        assert_eq!(card.to_reply(&ReplyIntent::Always), InputReply::Always);
        assert_eq!(card.to_reply(&ReplyIntent::Option(0)), InputReply::Once);
        assert_eq!(card.to_reply(&ReplyIntent::Option(1)), InputReply::Reject);
        assert_eq!(
            card.to_reply(&ReplyIntent::Text("typed".to_string())),
            InputReply::Value {
                value: "typed".to_string()
            }
        );
    }

    #[test]
    fn awaiting_tasks_filters_status_and_orders_newest_first() {
        let old = Uuid::new_v4();
        let new = Uuid::new_v4();
        let tasks = vec![
            make_task(
                old,
                "old",
                TaskStatus::AwaitingInput,
                "2026-01-01T00:00:00Z",
            ),
            make_task(
                new,
                "new",
                TaskStatus::AwaitingInput,
                "2026-01-02T00:00:00Z",
            ),
            make_task(
                Uuid::new_v4(),
                "done",
                TaskStatus::Succeeded,
                "2026-01-03T00:00:00Z",
            ),
        ];
        let rows = awaiting_tasks(&tasks);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, new);
        assert_eq!(rows[1].id, old);
    }

    #[test]
    fn recent_failures_orders_newest_first_and_caps() {
        let tasks: Vec<Task> = (0..8)
            .map(|i| {
                make_task(
                    Uuid::new_v4(),
                    "boom",
                    TaskStatus::Failed,
                    &format!("2026-01-0{}T00:00:00Z", i + 1),
                )
            })
            .collect();
        let rows = recent_failures(&tasks, FAILURES_LIMIT);
        assert_eq!(rows.len(), FAILURES_LIMIT);
        // Newest first: day 8 down to day 4.
        assert_eq!(rows[0].created_at.to_string(), "2026-01-08 00:00:00 UTC");
        assert_eq!(rows[4].created_at.to_string(), "2026-01-04 00:00:00 UTC");
    }

    #[test]
    fn recent_events_orders_descending_and_caps() {
        let events: Vec<Event> = (1..=30).map(event).collect();
        let rows = recent_events(&events, EVENTS_LIMIT);
        assert_eq!(rows.len(), EVENTS_LIMIT);
        assert_eq!(rows[0].id, 30);
        assert_eq!(rows[EVENTS_LIMIT - 1].id, 30 - EVENTS_LIMIT as i64 + 1);
    }

    #[test]
    fn awaiting_card_for_matches_the_session_by_task_id() {
        let task = make_task(
            Uuid::new_v4(),
            "implement",
            TaskStatus::AwaitingInput,
            "2026-01-01T00:00:00Z",
        );
        let agents = vec![agent_with_session(
            "sess-1",
            Some(&task.id.to_string()),
            Some(reason(Some("req-1"), vec!["Allow", "Deny"], true)),
        )];
        let card = awaiting_card_for(&task, &agents).expect("card");
        assert_eq!(card.session_id, "sess-1");
        assert_eq!(card.request_id.as_deref(), Some("req-1"));
        assert_eq!(card.kind, AwaitingInputKind::Permission);
        assert_eq!(card.options, vec!["Allow".to_string(), "Deny".to_string()]);
        assert!(card.allow_always);
        assert_eq!(card.message, "Allow this command?");

        // No matching session -> no card.
        let other = make_task(
            Uuid::new_v4(),
            "other",
            TaskStatus::AwaitingInput,
            "2026-01-01T00:00:00Z",
        );
        assert!(awaiting_card_for(&other, &agents).is_none());
    }

    #[test]
    fn awaiting_card_for_handles_a_screen_detected_prompt() {
        let task = make_task(
            Uuid::new_v4(),
            "implement",
            TaskStatus::AwaitingInput,
            "2026-01-01T00:00:00Z",
        );
        let agents = vec![agent_with_session(
            "sess-2",
            Some(&task.id.to_string()),
            Some(reason(None, vec![], false)),
        )];
        let card = awaiting_card_for(&task, &agents).expect("card");
        assert_eq!(card.request_id, None);
        assert!(card.options.is_empty());
        assert!(!card.allow_always);
    }

    #[test]
    fn pull_refresh_helpers() {
        assert!(pull_refresh_reached(10.0, 90.0, 70.0));
        assert!(!pull_refresh_reached(10.0, 70.0, 70.0));
        assert!(!pull_refresh_reached(90.0, 10.0, 70.0));
        assert_eq!(pull_distance(10.0, 90.0), 80.0);
        assert_eq!(pull_distance(90.0, 10.0), 0.0);
    }

    #[test]
    fn inbox_view_mounts_with_state() {
        #[component]
        fn Harness() -> Element {
            use_context_provider(AppState::new);
            rsx! {
                Router::<Route> {}
            }
        }
        let mut dom = VirtualDom::new(Harness);
        dom.rebuild_in_place();
    }
}
