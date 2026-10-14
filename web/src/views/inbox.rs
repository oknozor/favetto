//! The Inbox: the default landing view.
//!
//! It is a UI-only hub for everything that needs a human: the tasks blocked in
//! `AwaitingInput` (newest first) with their agent prompt and options, the most
//! recent failures, and the last 20 persisted events. The task/event data comes
//! from the shared [`AppState`] cache, seeded here with `tasks.list` /
//! `agents.list` / `events.tail` and kept live by the existing SSE reader, so a
//! `task.updated` or `event` push re-renders the page with no extra wiring.
//!
//! Answering a prompt (`agents.reply`) is wired in the follow-up issue #247, so
//! the option buttons and free-text control show a "not yet implemented" toast
//! for now; the [`AwaitingCard`] fields are kept ready for that follow-up.

use dioxus::prelude::*;
use favetto_wire::model::{AgentCatalogEntry, Event, Task, TaskStatus};
use favetto_wire::rpc::{EmptyParams, EventsTailParams, TasksListParams};
use uuid::Uuid;

use crate::app::Route;
use crate::client::RpcClient;
use crate::components::common::{EmptyState, EventBadge, Spinner};
use crate::components::task_card::TaskCard;
use crate::config;
use crate::state::{onboarding_step, AppState, OnboardingStep};
use crate::views::tasks::{format_age, now_ms, status_label};

/// How many recent failures the Inbox shows.
pub const FAILURES_LIMIT: usize = 5;
/// How many events the live feed shows.
pub const EVENTS_LIMIT: usize = 20;
/// Drag distance (px) that triggers a pull-to-refresh.
pub const PULL_THRESHOLD: f64 = 70.0;

/// Copy shown when a reply control is pressed; #247 replaces the stub with a
/// real `agents.reply` call.
pub const REPLY_NOT_IMPLEMENTED: &str = "Replying is not implemented yet (see #247).";

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
    /// The prompt text.
    pub message: String,
    /// Selectable options; empty means free-form text.
    pub options: Vec<String>,
    /// Whether an "always" answer is offered.
    pub allow_always: bool,
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
    // Bumped by the Refresh button and the pull-to-refresh gesture; the resource
    // below subscribes to it and re-runs.
    let mut refresh = use_signal(|| 0u64);
    let mut notice = use_signal(|| Option::<String>::None);
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
    let tasks: Vec<Task> = cache.tasks.values().cloned().collect();
    let awaiting = awaiting_tasks(&tasks);
    let failures = recent_failures(&tasks, FAILURES_LIMIT);
    let events = recent_events(&cache.events, EVENTS_LIMIT);
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
            if let Some(message) = notice() {
                div { class: "toast", role: "status", "{message}" }
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
                                name: task.name.clone(),
                                status: task.status,
                                age: format_age(task.created_at.timestamp_millis(), now),
                                card,
                                on_reply: move |_| notice.set(Some(REPLY_NOT_IMPLEMENTED.to_string())),
                            }
                        } else {
                            TaskCard {
                                key: "{task.id}",
                                id: task.id,
                                name: task.name.clone(),
                                status: task.status,
                                age: format_age(task.created_at.timestamp_millis(), now),
                                awaiting: true,
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
                            name: task.name.clone(),
                            status: task.status,
                            age: format_age(task.created_at.timestamp_millis(), now),
                            detail: task.error.clone(),
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

/// One awaiting-input card: the agent prompt plus its options (or a free-text
/// fallback). Every reply control calls `on_reply`; until #247 that only raises
/// the "not implemented" toast.
#[component]
fn AwaitingInputCard(
    id: Uuid,
    name: String,
    status: TaskStatus,
    age: String,
    card: AwaitingCard,
    on_reply: EventHandler<Option<String>>,
) -> Element {
    let mut draft = use_signal(String::new);
    let message = card.message.clone();
    let options = card.options.clone();
    let allow_always = card.allow_always;
    let screen_detected = card.request_id.is_none();
    let badge = format!("status-badge status-{}", status.as_str());

    rsx! {
        div { class: "awaiting-card",
            div { class: "awaiting-head",
                Link {
                    to: Route::TaskDetail { id: id.to_string() },
                    class: "awaiting-name",
                    "{name}"
                }
                span { class: badge, "{status_label(status)}" }
                span { class: "awaiting-age", "{age}" }
            }
            p { class: "awaiting-prompt", "{message}" }
            if screen_detected {
                p { class: "awaiting-hint",
                    "This prompt was detected from the terminal; reply with free text."
                }
            }
            if options.is_empty() {
                div { class: "awaiting-free-text",
                    input {
                        class: "awaiting-input",
                        r#type: "text",
                        placeholder: "Type a reply…",
                        value: "{draft}",
                        oninput: move |event| draft.set(event.value()),
                    }
                    button {
                        class: "btn primary option-button",
                        onclick: move |_| on_reply.call(Some(draft())),
                        "Send"
                    }
                }
            } else {
                div { class: "awaiting-options",
                    for option in options {
                        button {
                            class: "btn option-button",
                            onclick: move |_| on_reply.call(Some(option.clone())),
                            "{option}"
                        }
                    }
                    if allow_always {
                        button {
                            class: "btn option-button",
                            onclick: move |_| on_reply.call(None),
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
