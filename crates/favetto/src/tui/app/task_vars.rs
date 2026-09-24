//! Catalog-task variable form state and its input handler.

use super::*;

impl App {
    /// Start a catalog task, opening the variable form when it declares `[[vars]]`.
    pub(super) fn begin_catalog_task(&mut self, entry: &CatalogEntry) -> UiAction {
        if entry.vars.is_empty() {
            return UiAction::StartTask(entry.name.clone());
        }
        let values = entry
            .vars
            .iter()
            .map(|v| TextBuffer::new(v.default.clone().unwrap_or_default()))
            .collect();
        let choice_selected = entry
            .vars
            .iter()
            .map(|v| match (&v.choices, &v.default) {
                (Some(choices), Some(default)) => {
                    choices.iter().position(|c| c == default).unwrap_or(0)
                }
                _ => 0,
            })
            .collect();
        self.popup = Popup::TaskVars(TaskVarsForm {
            task: entry.name.clone(),
            vars: entry.vars.clone(),
            current: 0,
            values,
            choice_selected,
            error: None,
        });
        UiAction::None
    }

    pub(super) fn handle_task_vars_key(&mut self, key: KeyEvent) -> UiAction {
        // Take the form out so the borrow checker lets us restore/clear `self.popup`.
        let mut popup = std::mem::replace(&mut self.popup, Popup::None);
        let Popup::TaskVars(form) = &mut popup else {
            self.popup = popup;
            return UiAction::None;
        };

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let last = form.vars.len().saturating_sub(1);
        let has_choices = form
            .vars
            .get(form.current)
            .is_some_and(|v| v.choices.is_some());
        let multiline = form.vars.get(form.current).is_some_and(|v| v.multiline);

        let mut submit = false;
        match key.code {
            KeyCode::Esc => return UiAction::None, // popup already cleared
            KeyCode::Tab => {
                if form.current >= last {
                    submit = true;
                } else {
                    form.current += 1;
                }
            }
            KeyCode::BackTab => {
                form.current = form.current.saturating_sub(1);
            }
            KeyCode::Down if has_choices => {
                if let Some(choices) = form.vars[form.current].choices.as_ref() {
                    let selected = &mut form.choice_selected[form.current];
                    if !choices.is_empty() {
                        *selected = (*selected + 1).min(choices.len() - 1);
                    }
                }
            }
            KeyCode::Up if has_choices => {
                let selected = &mut form.choice_selected[form.current];
                *selected = selected.saturating_sub(1);
            }
            KeyCode::Down if multiline => {
                if let Some(value) = form.values.get_mut(form.current) {
                    value.line_down();
                }
            }
            KeyCode::Up if multiline => {
                if let Some(value) = form.values.get_mut(form.current) {
                    value.line_up();
                }
            }
            KeyCode::Down | KeyCode::Up => {
                if key.code == KeyCode::Down {
                    if form.current < last {
                        form.current += 1;
                    } else {
                        submit = true;
                    }
                } else {
                    form.current = form.current.saturating_sub(1);
                }
            }
            KeyCode::Enter if ctrl => submit = true,
            // Shift+Enter (and Alt+Enter) insert a newline in multiline fields;
            // many terminals report the same byte for Enter, hence the plain
            // Enter fallback below.
            KeyCode::Enter if multiline && (shift || alt) => {
                if let Some(value) = form.values.get_mut(form.current) {
                    value.insert_char('\n');
                }
            }
            KeyCode::Enter if multiline => {
                if let Some(value) = form.values.get_mut(form.current) {
                    value.insert_char('\n');
                }
            }
            KeyCode::Enter if alt => submit = true,
            KeyCode::Enter => {
                if form.current < last {
                    form.current += 1;
                } else {
                    submit = true;
                }
            }
            KeyCode::Backspace if !has_choices && alt => {
                if let Some(value) = form.values.get_mut(form.current) {
                    value.delete_word_before();
                }
            }
            KeyCode::Backspace if !has_choices => {
                if let Some(value) = form.values.get_mut(form.current) {
                    value.backspace();
                }
            }
            KeyCode::Delete if !has_choices => {
                if let Some(value) = form.values.get_mut(form.current) {
                    value.delete();
                }
            }
            KeyCode::Left if !has_choices && (alt || ctrl) => {
                if let Some(value) = form.values.get_mut(form.current) {
                    value.move_word_left();
                }
            }
            KeyCode::Left if !has_choices => {
                if let Some(value) = form.values.get_mut(form.current) {
                    value.move_left();
                }
            }
            KeyCode::Right if !has_choices && (alt || ctrl) => {
                if let Some(value) = form.values.get_mut(form.current) {
                    value.move_word_right();
                }
            }
            KeyCode::Right if !has_choices => {
                if let Some(value) = form.values.get_mut(form.current) {
                    value.move_right();
                }
            }
            KeyCode::Home if !has_choices => {
                if let Some(value) = form.values.get_mut(form.current) {
                    value.home();
                }
            }
            KeyCode::End if !has_choices => {
                if let Some(value) = form.values.get_mut(form.current) {
                    value.end();
                }
            }
            KeyCode::Char('a') if !has_choices && ctrl => {
                if let Some(value) = form.values.get_mut(form.current) {
                    value.home();
                }
            }
            KeyCode::Char('e') if !has_choices && ctrl => {
                if let Some(value) = form.values.get_mut(form.current) {
                    value.end();
                }
            }
            KeyCode::Char('u') if !has_choices && ctrl => {
                if let Some(value) = form.values.get_mut(form.current) {
                    value.delete_to_line_start();
                }
            }
            KeyCode::Char('k') if !has_choices && ctrl => {
                if let Some(value) = form.values.get_mut(form.current) {
                    value.delete_to_line_end();
                }
            }
            KeyCode::Char('w') if !has_choices && ctrl => {
                if let Some(value) = form.values.get_mut(form.current) {
                    value.delete_word_before();
                }
            }
            KeyCode::Char(c) if !ctrl && !alt && !has_choices => {
                if let Some(value) = form.values.get_mut(form.current) {
                    value.insert_char(c);
                }
            }
            _ => {}
        }

        if !submit {
            self.popup = popup;
            return UiAction::None;
        }

        match submit_task_vars(form) {
            Ok(input) => {
                let name = form.task.clone();
                UiAction::StartTaskWithInput { name, input }
            }
            Err(e) => {
                form.error = Some(e);
                self.popup = popup;
                UiAction::None
            }
        }
    }
}

/// Build the `input` object for a completed variable form. Required empty fields
/// and failed type coercions return an error string that keeps the form open.
fn submit_task_vars(form: &TaskVarsForm) -> Result<Value, String> {
    let mut map = serde_json::Map::new();
    for (i, var) in form.vars.iter().enumerate() {
        let raw = match &var.choices {
            Some(choices) => choices
                .get(form.choice_selected.get(i).copied().unwrap_or(0))
                .cloned()
                .unwrap_or_default(),
            None => form
                .values
                .get(i)
                .map(|buffer| buffer.value().to_string())
                .unwrap_or_default(),
        };
        if raw.is_empty() {
            if var.required {
                return Err(format!("{} is required", var.prompt));
            }
            continue;
        }
        let value = var
            .coerce(&raw)
            .map_err(|e| format!("{}: {e}", var.prompt))?;
        map.insert(var.name.clone(), value);
    }
    Ok(Value::Object(map))
}
