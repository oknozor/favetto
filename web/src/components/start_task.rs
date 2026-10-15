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
    // The task name is rendered through the uncontrolled `initial_value`, so the
    // original prop must survive the signal shadowing below. Typing must never
    // re-assign the DOM `value` (that resets the caret/IME on Android), so the
    // signal is only read on submit.
    let initial_name = name.clone();
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
                    // Uncontrolled: `initial_value` only seeds the field, so
                    // typing mid-text never rewrites the DOM value (which would
                    // reset the caret/IME on Android). `oninput` keeps the
                    // signal current for `submit`.
                    initial_value: "{initial_name}",
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
                                // Uncontrolled: seed with the declared default;
                                // `oninput` keeps `raw_vars` as the submit source.
                                initial_value: "{var.default.clone().unwrap_or_default()}",
                                oninput: move |event| {
                                    raw_vars.write()[position] = event.value();
                                },
                            }
                        } else {
                            input {
                                class: "input",
                                r#type: input_type(var.var_type),
                                initial_value: "{var.default.clone().unwrap_or_default()}",
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
                        // Uncontrolled: no initial content, and `input` is kept
                        // current by `oninput` for `submit`.
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

    // -- Mutation-level regression coverage for the Android caret jump -------
    //
    // Every text field in the dialog must be *uncontrolled*: the rendered
    // mutations never assign the DOM `value` property after input. On
    // Android/desktop Dioxus replays edits per animation frame, so re-assigning
    // `value` resets the caret to the end of the field and tears down the IME
    // composition region. These tests inspect the mutation stream directly: a
    // regression that restores `value:` (a controlled field) reappears here as a
    // `SetAttribute { name: "value" }` edit.

    /// Install the host-side form-event converter that the native/wasm
    /// renderers register before delivering DOM events.
    fn install_event_converter() {
        dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
    }

    /// Dispatch a synthetic `oninput` carrying `value` to `element`.
    fn dispatch_input(dom: &mut VirtualDom, element: dioxus::core::ElementId, value: &str) {
        let data = dioxus::html::SerializedFormData::new(value.to_string(), Vec::new());
        let payload = dioxus::html::PlatformEventData::new(Box::new(data));
        let event: Event<dyn std::any::Any> = Event::new(std::rc::Rc::new(payload), true);
        // `NewEventListener`/`handle_event` use the bare event name (`input`),
        // without the `on` attribute prefix.
        dom.runtime().handle_event("input", event, element);
    }

    /// The elements in `mutations` that registered an `oninput` listener.
    fn oninput_targets(mutations: &dioxus::core::Mutations) -> Vec<dioxus::core::ElementId> {
        mutations
            .edits
            .iter()
            .filter_map(|mutation| match mutation {
                dioxus::core::Mutation::NewEventListener { name, id } if name == "input" => {
                    Some(*id)
                }
                _ => None,
            })
            .collect()
    }

    /// True when any mutation assigns the controlled DOM `value` property.
    fn writes_dom_value(mutations: &dioxus::core::Mutations) -> bool {
        mutations.edits.iter().any(|mutation| {
            matches!(mutation, dioxus::core::Mutation::SetAttribute { name, .. } if *name == "value")
        })
    }

    /// True when `expected` is rendered as an uncontrolled `initial_value`.
    fn has_initial_value(mutations: &dioxus::core::Mutations, expected: &str) -> bool {
        mutations.edits.iter().any(|mutation| {
            matches!(
                mutation,
                dioxus::core::Mutation::SetAttribute {
                    name,
                    value: dioxus::core::AttributeValue::Text(text),
                    ..
                } if *name == "initial_value" && text == expected
            )
        })
    }

    #[test]
    fn start_task_fields_are_uncontrolled() {
        use crate::state::AppState;

        install_event_converter();
        let mut dom = VirtualDom::new(|| {
            use_context_provider(AppState::new);
            rsx! {
                StartTaskDialog {
                    name: "implement_issue".to_string(),
                    vars: Vec::new(),
                    on_close: move |_| {},
                    on_started: move |_| {},
                }
            }
        });

        let initial = dom.rebuild_to_vec();
        assert!(
            has_initial_value(&initial, "implement_issue"),
            "the prefilled task name must render as initial_value: {:?}",
            initial.edits
        );
        assert!(
            !writes_dom_value(&initial),
            "mounting must not assign the controlled value property: {:?}",
            initial.edits
        );

        let targets = oninput_targets(&initial);
        assert!(!targets.is_empty(), "the dialog has oninput fields");
        for element in targets {
            dispatch_input(&mut dom, element, "implement_issue_mid");
            let edits = dom.render_immediate_to_vec();
            assert!(
                !writes_dom_value(&edits),
                "typing must not reassign the DOM value (caret jump): {:?}",
                edits.edits
            );
        }
    }

    fn vars_with_defaults() -> Vec<TaskVar> {
        let mut branch = var("branch", VarType::String, false);
        branch.default = Some("main".to_string());
        let mut body = var("body", VarType::String, false);
        body.multiline = true;
        vec![branch, body]
    }

    #[test]
    fn start_task_typed_var_fields_are_uncontrolled() {
        use crate::state::AppState;

        install_event_converter();
        let mut dom = VirtualDom::new(|| {
            use_context_provider(AppState::new);
            rsx! {
                StartTaskDialog {
                    name: "implement_issue".to_string(),
                    vars: vars_with_defaults(),
                    on_close: move |_| {},
                    on_started: move |_| {},
                }
            }
        });

        let initial = dom.rebuild_to_vec();
        assert!(
            has_initial_value(&initial, "main"),
            "a var default must render as initial_value: {:?}",
            initial.edits
        );
        assert!(
            !writes_dom_value(&initial),
            "mounting must not assign the controlled value property: {:?}",
            initial.edits
        );

        for element in oninput_targets(&initial) {
            dispatch_input(&mut dom, element, "typed");
            let edits = dom.render_immediate_to_vec();
            assert!(
                !writes_dom_value(&edits),
                "typing must not reassign the DOM value (caret jump): {:?}",
                edits.edits
            );
        }
    }
}
