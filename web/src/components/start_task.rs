//! The shared `tasks.start` dialog, used by both the Tasks list and the
//! Catalog.
//!
//! The name field autocompletes over `catalog.list`. A task that declares
//! `[[vars]]` renders one typed field per variable (matching the TUI's start
//! form) and composes the JSON `input` with [`TaskVar::coerce`]; every other
//! task falls back to the free-form JSON textarea.

use dioxus::prelude::*;
use favetto_wire::model::Task;
use favetto_wire::rpc::{EmptyParams, TasksStartParams};
use favetto_wire::task_var::{TaskVar, VarType};

use crate::client::RpcClient;
use crate::components::dialog::Dialog;
use crate::state::AppState;

/// Parse the start dialog's free-form `input` textarea. Empty/whitespace means
/// "no input"; anything else must be valid JSON.
pub fn parse_task_input(raw: &str) -> Result<Option<serde_json::Value>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(trimmed)
        .map(Some)
        .map_err(|err| format!("input must be valid JSON: {err}"))
}

/// Compose a task `input` object from typed `[[vars]]` values.
///
/// `raw` holds one entry per var (in `vars` order); a missing entry falls back
/// to the var's `default`. A blank optional var is omitted from `input`; a
/// blank required var is an error. Each value goes through [`TaskVar::coerce`],
/// so `int`/`bool` vars become JSON numbers/booleans and `choices` are enforced.
pub fn input_from_vars(
    vars: &[TaskVar],
    raw: &[String],
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let mut input = serde_json::Map::new();
    for (position, var) in vars.iter().enumerate() {
        let value = raw
            .get(position)
            .cloned()
            .or_else(|| var.default.clone())
            .unwrap_or_default();
        if value.trim().is_empty() && !var.required {
            continue;
        }
        let coerced = var
            .coerce(&value)
            .map_err(|err| format!("{}: {err}", var.prompt))?;
        input.insert(var.name.clone(), coerced);
    }
    Ok(input)
}

/// The HTML `type` for a typed variable's input.
fn input_type(var_type: VarType) -> &'static str {
    match var_type {
        VarType::String => "text",
        VarType::Int => "number",
        VarType::Bool => "text",
    }
}

/// A short type hint rendered under a typed field.
fn var_type_hint(var_type: VarType) -> &'static str {
    match var_type {
        VarType::String => "",
        VarType::Int => "integer",
        VarType::Bool => "true or false",
    }
}

/// The `tasks.start` dialog: a catalog-name autocomplete plus either a typed
/// `[[vars]]` form (when `vars` is non-empty) or an optional JSON textarea.
#[component]
pub fn StartTaskDialog(
    /// Pre-filled task name (empty for a free start).
    name: String,
    /// The catalog entry's `[[vars]]`, if any.
    vars: Vec<TaskVar>,
    on_close: EventHandler<()>,
    on_started: EventHandler<Task>,
) -> Element {
    let state = use_context::<AppState>();
    let has_vars = !vars.is_empty();
    let mut name = use_signal(move || name);
    let mut input = use_signal(String::new);
    let defaults: Vec<String> = vars
        .iter()
        .map(|var| var.default.clone().unwrap_or_default())
        .collect();
    let mut raw_vars = use_signal(move || defaults);
    let submit_vars = vars.clone();
    let mut error = use_signal(|| None::<String>);
    let mut submitting = use_signal(|| false);

    // Catalog names power the `<datalist>` autocomplete. A failure here is not
    // fatal: the user can still type any task name. Repo-local names are
    // returned already qualified (`repo:<name>/<path>`), so starting one runs it
    // in the repo's cwd.
    let catalog = use_resource(move || {
        let base = (state.daemon_url)();
        let token = (state.token)();
        async move {
            RpcClient::new(base)
                .with_token(token)
                .catalog_list(EmptyParams {})
                .await
                .map(|entries| {
                    entries
                        .into_iter()
                        .map(|entry| entry.name)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        }
    });
    let names = catalog().unwrap_or_default();

    let submit = move |_| {
        let trimmed = name().trim().to_string();
        if trimmed.is_empty() {
            error.set(Some("task name is required".to_string()));
            return;
        }
        let parsed = if has_vars {
            match input_from_vars(&submit_vars, &raw_vars()) {
                Ok(input) => Some(serde_json::Value::Object(input)),
                Err(err) => {
                    error.set(Some(err));
                    return;
                }
            }
        } else {
            match parse_task_input(&input()) {
                Ok(value) => value,
                Err(err) => {
                    error.set(Some(err));
                    return;
                }
            }
        };
        error.set(None);
        submitting.set(true);
        spawn(async move {
            let client = RpcClient::new((state.daemon_url)()).with_token((state.token)());
            let params = TasksStartParams {
                name: trimmed,
                input: parsed,
                interactive: None,
            };
            match client.tasks_start(params).await {
                Ok(task) => {
                    submitting.set(false);
                    on_started.call(task);
                }
                Err(err) => {
                    submitting.set(false);
                    error.set(Some(err.to_string()));
                }
            }
        });
    };

    rsx! {
        Dialog { title: "Start task".to_string(), on_close,
            label { class: "field",
                span { class: "field-label", "Task name" }
                input {
                    class: "input",
                    list: "favetto-catalog-names",
                    value: "{name}",
                    placeholder: "e.g. implement_issue",
                    oninput: move |event| name.set(event.value()),
                }
            }
            datalist { id: "favetto-catalog-names",
                for task_name in names {
                    option { key: "{task_name}", value: "{task_name}" }
                }
            }
            if has_vars {
                for (position, var) in vars.iter().enumerate() {
                    label { class: "field", key: "{var.name}",
                        span { class: "field-label",
                            "{var.prompt}"
                            if var.required {
                                span { class: "field-required", " *" }
                            }
                        }
                        if let Some(choices) = &var.choices {
                            select {
                                class: "input",
                                value: "{raw_vars()[position]}",
                                onchange: move |event| {
                                    raw_vars.write()[position] = event.value();
                                },
                                if !var.required {
                                    option { value: "", "" }
                                }
                                for choice in choices {
                                    option { key: "{choice}", value: "{choice}", "{choice}" }
                                }
                            }
                        } else if var.multiline {
                            textarea {
                                class: "input",
                                value: "{raw_vars()[position]}",
                                oninput: move |event| {
                                    raw_vars.write()[position] = event.value();
                                },
                            }
                        } else {
                            input {
                                class: "input",
                                r#type: input_type(var.var_type),
                                value: "{raw_vars()[position]}",
                                placeholder: "{var.default.clone().unwrap_or_default()}",
                                oninput: move |event| {
                                    raw_vars.write()[position] = event.value();
                                },
                            }
                        }
                        if !var_type_hint(var.var_type).is_empty() {
                            span { class: "field-hint", "{var_type_hint(var.var_type)}" }
                        }
                    }
                }
            } else {
                label { class: "field",
                    span { class: "field-label", "Input (JSON, optional)" }
                    textarea {
                        class: "input",
                        value: "{input}",
                        placeholder: "Optional JSON input",
                        oninput: move |event| input.set(event.value()),
                    }
                }
            }
            if let Some(err) = error() {
                p { class: "dialog-error", role: "alert", "{err}" }
            }
            div { class: "dialog-actions",
                button {
                    class: "btn",
                    disabled: submitting(),
                    onclick: move |_| on_close.call(()),
                    "Cancel"
                }
                button {
                    class: "btn primary",
                    disabled: submitting(),
                    onclick: submit,
                    "Start"
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn var(name: &str, var_type: VarType, required: bool) -> TaskVar {
        TaskVar {
            name: name.to_string(),
            prompt: name.to_string(),
            default: None,
            required,
            multiline: false,
            var_type,
            choices: None,
        }
    }

    #[test]
    fn parse_task_input_accepts_empty_and_rejects_invalid() {
        assert_eq!(parse_task_input(""), Ok(None));
        assert_eq!(parse_task_input("   "), Ok(None));
        assert_eq!(
            parse_task_input(r#"{"issue_id":247}"#),
            Ok(Some(serde_json::json!({ "issue_id": 247 })))
        );
        assert!(parse_task_input("not json").is_err());
    }

    #[test]
    fn input_from_vars_coerces_each_declared_type() {
        let vars = [
            var("name", VarType::String, false),
            var("count", VarType::Int, false),
            var("flag", VarType::Bool, false),
        ];
        let raw = vec!["hi".to_string(), " 3 ".to_string(), "true".to_string()];
        let input = input_from_vars(&vars, &raw).unwrap();
        assert_eq!(input["name"], serde_json::json!("hi"));
        assert_eq!(input["count"], serde_json::json!(3));
        assert_eq!(input["flag"], serde_json::json!(true));
    }

    #[test]
    fn input_from_vars_enforces_required_and_reports_bad_values() {
        let required = [var("issue", VarType::Int, true)];
        let err = input_from_vars(&required, &["".to_string()]).unwrap_err();
        assert!(err.contains("issue"), "{err}");

        let err = input_from_vars(&required, &["nope".to_string()]).unwrap_err();
        assert!(err.contains("integer"), "{err}");
    }

    #[test]
    fn input_from_vars_omits_blank_optional_fields() {
        let vars = [var("maybe", VarType::String, false)];
        let input = input_from_vars(&vars, &["   ".to_string()]).unwrap();
        assert!(input.is_empty());
    }

    #[test]
    fn input_from_vars_falls_back_to_the_default() {
        let mut optional = var("branch", VarType::String, false);
        optional.default = Some("main".to_string());
        let input = input_from_vars(&[optional], &[]).unwrap();
        assert_eq!(input["branch"], serde_json::json!("main"));
    }

    #[test]
    fn start_task_dialog_mounts_with_state() {
        use crate::state::AppState;

        let mut dom = VirtualDom::new(|| {
            use_context_provider(AppState::new);
            rsx! {
                StartTaskDialog {
                    name: "triage_issues".to_string(),
                    vars: Vec::new(),
                    on_close: move |_| {},
                    on_started: move |_| {},
                }
            }
        });
        dom.rebuild_in_place();
    }

    fn typed_vars() -> Vec<TaskVar> {
        vec![
            var("name", VarType::String, true),
            var("count", VarType::Int, false),
        ]
    }

    #[test]
    fn start_task_dialog_mounts_with_typed_vars() {
        use crate::state::AppState;

        let mut dom = VirtualDom::new(|| {
            use_context_provider(AppState::new);
            rsx! {
                StartTaskDialog {
                    name: String::new(),
                    vars: typed_vars(),
                    on_close: move |_| {},
                    on_started: move |_| {},
                }
            }
        });
        dom.rebuild_in_place();
    }
}
