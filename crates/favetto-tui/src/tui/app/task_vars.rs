//! Manual task-input (`[[vars]]`) form handling for [`App`](super::App).

use super::*;

impl App {
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
