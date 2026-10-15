//! Schedules view (`/schedules`): list, create, edit and delete cron schedules.
//!
//! The wire `Schedule` has no human `name` field: its `id` is the stable key the
//! daemon persists (and generates as a UUID when a new schedule omits one), so
//! the form exposes it as an optional "Name / ID". The list shows the cron
//! expression, the catalog task it enqueues, whether it is enabled, its last run
//! and a live countdown to its next run. The daemon computes `next_run` in UTC
//! for enabled schedules; disabled schedules (and expressions the daemon cannot
//! parse) report no next run. A one-second client-side clock drives the
//! countdown, and the list re-fetches on each `cron_tick` so it resets after a
//! fire.
//!
//! Validation is done client-side before submitting so a typo fails without a
//! round trip: the cron is checked against the daemon's `croner` format (five to
//! seven whitespace-separated fields; the catalog examples use six with seconds
//! first) and the optional `input` must be valid JSON.

use dioxus::prelude::*;
use favetto_wire::model::Schedule;
use favetto_wire::rpc::{EmptyParams, ScheduleDeleteParams, ScheduleUpsertParams};

use crate::client::RpcClient;
use crate::state::{task_display_name, AppState};

/// The raw text fields of the create/edit form.
#[derive(Clone, PartialEq)]
pub struct ScheduleForm {
    /// The stable schedule id. Blank on create lets the daemon generate one.
    pub id: String,
    pub cron: String,
    pub task: String,
    /// Optional JSON object enqueued with each run.
    pub input: String,
    pub enabled: bool,
}

impl Default for ScheduleForm {
    fn default() -> Self {
        Self {
            id: String::new(),
            // A daily 08:00 default, matching the catalog examples
            // (`0 0 8 * * *`).
            cron: "0 0 8 * * *".to_string(),
            task: String::new(),
            input: String::new(),
            enabled: true,
        }
    }
}

impl ScheduleForm {
    /// Pre-fill the form from an existing schedule for editing.
    pub fn from_schedule(schedule: &Schedule) -> Self {
        let input = if schedule.input.is_null() {
            String::new()
        } else {
            serde_json::to_string(&schedule.input).unwrap_or_default()
        };
        Self {
            id: schedule.id.clone(),
            cron: schedule.cron.clone(),
            task: schedule.task.clone(),
            input,
            enabled: schedule.enabled,
        }
    }
}

/// Whether every cron field uses the syntax the daemon's parser accepts.
///
/// Deliberately permissive (the daemon re-validates with the `cron` crate); the
/// goal is to catch an empty or obviously malformed expression early.
fn valid_cron_field(field: &str) -> bool {
    !field.is_empty()
        && field.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '*' | '?' | ',' | '-' | '/' | '#' | 'L' | 'W' | '+')
        })
}

/// Validate a cron expression and return its trimmed form.
///
/// `tokio-cron-scheduler` (the daemon's parser, `croner`) accepts five to seven
/// whitespace-separated fields: `minute hour day month weekday`, with an
/// optional leading `second` and trailing `year`.
pub fn validate_cron(expr: &str) -> Result<String, String> {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        return Err("cron expression is required".into());
    }
    let fields: Vec<&str> = trimmed.split_whitespace().collect();
    if !(5..=7).contains(&fields.len()) {
        return Err(
            "cron must have between 5 and 7 fields: minute hour day month weekday [second] [year]"
                .to_string(),
        );
    }
    for field in fields {
        if !valid_cron_field(field) {
            return Err(format!("invalid cron field '{field}'"));
        }
    }
    Ok(trimmed.to_string())
}

/// Parse the optional `input` textarea: empty means "no input", anything else
/// must be valid JSON.
pub fn parse_schedule_input(raw: &str) -> Result<Option<serde_json::Value>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(trimmed)
        .map(Some)
        .map_err(|err| format!("input must be valid JSON: {err}"))
}

/// Validate a raw form into `schedules.upsert` params.
pub fn build_schedule_params(form: &ScheduleForm) -> Result<ScheduleUpsertParams, String> {
    let task = form.task.trim();
    if task.is_empty() {
        return Err("task name is required".into());
    }
    let cron = validate_cron(&form.cron)?;
    let input = parse_schedule_input(&form.input)?;
    let id = form.id.trim();
    Ok(ScheduleUpsertParams {
        id: (!id.is_empty()).then(|| id.to_string()),
        cron,
        task: task.to_string(),
        input,
        enabled: Some(form.enabled),
    })
}

/// `in 3h 12m` / `in 45s` / `in 2d 4h`; `disabled` when the schedule is off,
/// an em dash when there is no computed next run. Epoch-based so the caller does
/// not need a `chrono` dependency.
pub fn format_next_run(enabled: bool, next_run_epoch: Option<i64>, now_epoch: i64) -> String {
    if !enabled {
        return "disabled".to_string();
    }
    let Some(next) = next_run_epoch else {
        return "—".to_string();
    };
    let secs = (next - now_epoch).max(0);
    if secs < 60 {
        format!("in {secs}s")
    } else if secs < 3_600 {
        format!("in {}m", secs / 60)
    } else if secs < 86_400 {
        format!("in {}h {}m", secs / 3_600, (secs % 3_600) / 60)
    } else {
        format!("in {}d {}h", secs / 86_400, (secs % 86_400) / 3_600)
    }
}

/// A bearer client built from the shared state.
fn client(state: AppState) -> RpcClient {
    RpcClient::new((state.daemon_url)()).with_token((state.token)())
}

/// The `/schedules` list with create/edit/delete actions.
#[component]
pub fn Schedules() -> Element {
    let state = use_context::<AppState>();
    let mut reload = use_signal(|| 0u64);
    let mut error = use_signal(|| Option::<String>::None);
    let mut notice = use_signal(|| Option::<String>::None);
    let mut editing = use_signal(|| Option::<ScheduleForm>::None);
    let mut deleting = use_signal(|| Option::<String>::None);
    // A one-second clock drives the countdown; the list itself is only re-fetched
    // on mutation or `cron_tick`. Host tests have no ticking runtime, so the
    // clock is only bound mutably where a ticker exists.
    #[cfg(any(target_arch = "wasm32", target_os = "android"))]
    let mut now = use_signal(super::workflow::now_epoch);
    #[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
    let now = use_signal(super::workflow::now_epoch);

    #[cfg(target_arch = "wasm32")]
    use_future(move || async move {
        loop {
            gloo_timers::future::TimeoutFuture::new(1_000).await;
            now.set(super::workflow::now_epoch());
        }
    });
    #[cfg(all(not(target_arch = "wasm32"), target_os = "android"))]
    use_future(move || async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            now.set(super::workflow::now_epoch());
        }
    });

    // Reading `reload()` inside the future re-runs the fetch after a mutation;
    // reading `schedules_version` re-runs it after each `cron_tick`.
    let schedules = use_resource(move || {
        let base = (state.daemon_url)();
        let token = (state.token)();
        let _ = reload();
        let _ = (state.schedules_version)();
        async move {
            RpcClient::new(base)
                .with_token(token)
                .schedules_list(EmptyParams {})
                .await
                .map_err(|e| e.to_string())
        }
    });

    let rows: Vec<Schedule> = match schedules() {
        Some(Ok(list)) => list,
        _ => Vec::new(),
    };

    rsx! {
        main { id: "favetto-app", class: "settings-view",
            h1 { "Schedules" }
            p { class: "settings-hint",
                "Cron schedules that enqueue a catalog task on each fire."
            }
            div { class: "settings-toolbar",
                button {
                    class: "btn primary",
                    onclick: move |_| {
                        error.set(None);
                        notice.set(None);
                        editing.set(Some(ScheduleForm::default()));
                    },
                    "New schedule"
                }
            }
            if let Some(message) = error() {
                div { class: "error-panel", role: "alert", "{message}" }
            }
            if let Some(message) = notice() {
                p { class: "settings-success", role: "status", "{message}" }
            }
            match schedules() {
                None => rsx! { p { class: "loading", "Loading schedules…" } },
                Some(Err(err)) => rsx! {
                    div { class: "error-panel", "Could not load schedules: {err}" }
                },
                Some(Ok(_)) => rsx! {
                    if rows.is_empty() {
                        p { class: "empty-state", "No schedules yet." }
                    } else {
                        table { class: "settings-table",
                            thead {
                                tr {
                                    th { "Name / ID" }
                                    th { "Cron" }
                                    th { "Task" }
                                    th { "Enabled" }
                                    th { "Next run" }
                                    th { "Last run" }
                                    th { "Actions" }
                                }
                            }
                            tbody {
                                for schedule in rows {
                                    tr { key: "{schedule.id}",
                                        td { span { class: "schedule-id", "{schedule.id}" } }
                                        td { code { class: "schedule-cron", "{schedule.cron}" } }
                                        td { "{task_display_name(&schedule.task)}" }
                                        td {
                                            span {
                                                class: if schedule.enabled { "status-badge status-succeeded" } else { "status-badge status-cancelled" },
                                                if schedule.enabled { "enabled" } else { "disabled" }
                                            }
                                        }
                                        td { "{format_next_run(schedule.enabled, schedule.next_run.map(|t| t.timestamp()), now())}" }
                                        td { "{schedule.last_run.as_ref().map(|t| t.to_string()).unwrap_or_else(|| \"never\".to_string())}" }
                                        td { div { class: "row-actions",
                                            button {
                                                class: "btn",
                                                onclick: {
                                                    let form = ScheduleForm::from_schedule(&schedule);
                                                    move |_| {
                                                        error.set(None);
                                                        notice.set(None);
                                                        editing.set(Some(form.clone()));
                                                    }
                                                },
                                                "Edit"
                                            }
                                            button {
                                                class: "btn danger",
                                                onclick: {
                                                    let id = schedule.id.clone();
                                                    move |_| {
                                                        error.set(None);
                                                        notice.set(None);
                                                        deleting.set(Some(id.clone()));
                                                    }
                                                },
                                                "Delete"
                                            }
                                        } }
                                    }
                                }
                            }
                        }
                    }
                },
            }

            if let Some(form) = editing() {
                ScheduleDialog {
                    initial: form,
                    on_close: move |_| editing.set(None),
                    on_saved: move |_| {
                        editing.set(None);
                        notice.set(Some("Schedule saved.".into()));
                        error.set(None);
                        reload.set(reload() + 1);
                    },
                }
            }
            if let Some(id) = deleting() {
                ConfirmDialog {
                    message: format!("Delete schedule '{id}'?"),
                    confirm_label: "Delete",
                    on_cancel: move |_| deleting.set(None),
                    on_confirm: move |_| {
                        let id = id.clone();
                        spawn(async move {
                            match client(state).schedules_delete(ScheduleDeleteParams { id }).await {
                                Ok(_) => {
                                    deleting.set(None);
                                    notice.set(Some("Schedule deleted.".into()));
                                    error.set(None);
                                    reload.set(reload() + 1);
                                }
                                Err(err) => {
                                    deleting.set(None);
                                    error.set(Some(err.to_string()));
                                }
                            }
                        });
                    },
                }
            }
        }
    }
}

/// Create/edit dialog for one schedule (`schedules.upsert`).
#[component]
fn ScheduleDialog(
    initial: ScheduleForm,
    on_close: EventHandler<()>,
    on_saved: EventHandler<()>,
) -> Element {
    let state = use_context::<AppState>();
    let mut form = use_signal(|| initial);
    let mut error = use_signal(|| Option::<String>::None);
    let mut submitting = use_signal(|| false);

    let submit = move |_| {
        let params = match build_schedule_params(&form()) {
            Ok(params) => params,
            Err(err) => {
                error.set(Some(err));
                return;
            }
        };
        error.set(None);
        submitting.set(true);
        spawn(async move {
            match client(state).schedules_upsert(params).await {
                Ok(_) => {
                    submitting.set(false);
                    on_saved.call(());
                }
                Err(err) => {
                    submitting.set(false);
                    error.set(Some(err.to_string()));
                }
            }
        });
    };

    let error_value = error();
    let is_edit = !form().id.is_empty();
    rsx! {
        div { class: "spawn-overlay",
            div { class: "spawn-dialog", role: "dialog", "aria-modal": "true",
                h2 { if is_edit { "Edit schedule" } else { "New schedule" } }
                label { class: "field",
                    span { class: "field-label", "Name / ID (optional)" }
                    input {
                        class: "input",
                        value: "{form().id}",
                        placeholder: "daily-summary",
                        oninput: move |event| form.write().id = event.value(),
                    }
                }
                label { class: "field",
                    span { class: "field-label", "Cron (minute hour day month weekday, optional second/year)" }
                    input {
                        class: "input",
                        value: "{form().cron}",
                        placeholder: "0 0 8 * * *",
                        oninput: move |event| form.write().cron = event.value(),
                    }
                }
                label { class: "field",
                    span { class: "field-label", "Task name" }
                    input {
                        class: "input",
                        value: "{form().task}",
                        placeholder: "examples/scheduled/daily-summary",
                        oninput: move |event| form.write().task = event.value(),
                    }
                }
                label { class: "field",
                    span { class: "field-label", "Input (JSON, optional)" }
                    textarea {
                        class: "input",
                        value: "{form().input}",
                        placeholder: "{{}}",
                        oninput: move |event| form.write().input = event.value(),
                    }
                }
                label { class: "field field-inline",
                    input {
                        r#type: "checkbox",
                        checked: form().enabled,
                        onchange: move |event| form.write().enabled = event.checked(),
                    }
                    span { class: "field-label", "Enabled" }
                }
                if let Some(err) = error_value {
                    p { class: "spawn-error", role: "alert", "{err}" }
                }
                div { class: "spawn-dialog-actions",
                    button { class: "btn", disabled: submitting(), onclick: move |_| on_close.call(()), "Cancel" }
                    button { class: "btn primary", disabled: submitting(), onclick: submit, "Save" }
                }
            }
        }
    }
}

/// A minimal confirmation dialog used before a destructive action.
#[component]
fn ConfirmDialog(
    message: String,
    confirm_label: String,
    on_cancel: EventHandler<()>,
    on_confirm: EventHandler<()>,
) -> Element {
    rsx! {
        div { class: "spawn-overlay",
            div { class: "spawn-dialog confirm-dialog", role: "alertdialog", "aria-modal": "true",
                p { "{message}" }
                div { class: "spawn-dialog-actions",
                    button { class: "btn", onclick: move |_| on_cancel.call(()), "Cancel" }
                    button { class: "btn danger", onclick: move |_| on_confirm.call(()), "{confirm_label}" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_cron_accepts_five_to_seven_fields() {
        assert_eq!(validate_cron(" 0 0 8 * * * ").unwrap(), "0 0 8 * * *");
        assert!(validate_cron("0 8 * * *").is_ok());
        assert!(validate_cron("0 30 9 * * 1 2026").is_ok());
    }

    #[test]
    fn validate_cron_rejects_empty_wrong_arity_and_bad_chars() {
        assert!(validate_cron("").is_err());
        assert!(validate_cron("0 0 8 *").is_err());
        assert!(validate_cron("0 0 8 * * * * * *").is_err());
        assert!(validate_cron("0 0 8 * * ?").is_ok());
        assert!(validate_cron("0 0 8 * * $").is_err());
    }

    #[test]
    fn build_schedule_params_requires_task_and_valid_cron() {
        let form = ScheduleForm {
            task: "  ".into(),
            ..ScheduleForm::default()
        };
        assert!(build_schedule_params(&form).unwrap_err().contains("task"));

        let form = ScheduleForm {
            task: "t".into(),
            cron: "not a cron".into(),
            ..ScheduleForm::default()
        };
        assert!(build_schedule_params(&form).is_err());
    }

    #[test]
    fn build_schedule_params_parses_input_and_omits_blank_id() {
        let form = ScheduleForm {
            id: "  ".into(),
            task: "daily".into(),
            input: r#"{"repo":"Favetto"}"#.into(),
            ..ScheduleForm::default()
        };
        let params = build_schedule_params(&form).unwrap();
        assert_eq!(params.id, None);
        assert_eq!(params.task, "daily");
        assert_eq!(params.cron, "0 0 8 * * *");
        assert_eq!(params.enabled, Some(true));
        assert_eq!(params.input, Some(serde_json::json!({ "repo": "Favetto" })));
    }

    #[test]
    fn parse_schedule_input_rejects_invalid_json() {
        assert_eq!(parse_schedule_input("   ").unwrap(), None);
        assert!(parse_schedule_input("{not json}").is_err());
    }

    #[test]
    fn form_round_trips_from_a_schedule() {
        let schedule = Schedule {
            id: "daily".into(),
            cron: "0 0 8 * * *".into(),
            task: "t".into(),
            input: serde_json::json!({ "a": 1 }),
            enabled: false,
            last_run: None,
            next_run: None,
        };
        let form = ScheduleForm::from_schedule(&schedule);
        assert_eq!(form.id, "daily");
        assert!(!form.enabled);
        assert_eq!(form.input, r#"{"a":1}"#);
    }

    #[test]
    fn format_next_run_covers_disabled_unknown_and_buckets() {
        let now = 1_000_000;
        assert_eq!(format_next_run(false, Some(now + 30), now), "disabled");
        assert_eq!(format_next_run(true, None, now), "—");
        assert_eq!(format_next_run(true, Some(now + 45), now), "in 45s");
        assert_eq!(
            format_next_run(true, Some(now + 3_600 * 3 + 720), now),
            "in 3h 12m"
        );
        assert_eq!(
            format_next_run(true, Some(now + 86_400 * 2 + 3_600 * 4), now),
            "in 2d 4h"
        );
        // A skewed clock (next run already past) degrades to `in 0s`.
        assert_eq!(format_next_run(true, Some(now - 10), now), "in 0s");
    }

    fn with_state(view: fn() -> Element) -> Element {
        use_context_provider(AppState::new);
        view()
    }

    #[test]
    fn schedules_view_mounts_with_state() {
        let mut dom = VirtualDom::new(|| with_state(|| rsx! { Schedules {} }));
        dom.rebuild_in_place();
    }
}
