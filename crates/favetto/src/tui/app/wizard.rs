//! One-shot task wizard input handling for [`App`](super::App).

use super::*;

impl App {
    pub(super) fn handle_wizard_key(&mut self, key: KeyEvent) -> UiAction {
        // Take the wizard out so the borrow checker lets us mutate `self.popup`.
        let mut popup = std::mem::replace(&mut self.popup, Popup::None);
        let Popup::Wizard(w) = &mut popup else {
            self.popup = popup;
            return UiAction::None;
        };

        if key.code == KeyCode::Esc {
            return UiAction::None; // popup already cleared
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        let action = match key.code {
            KeyCode::Up => {
                if w.step == WizardStep::Agent {
                    w.move_selection(false);
                } else {
                    w.selected = w.selected.saturating_sub(1);
                }
                UiAction::None
            }
            KeyCode::Down => {
                if w.step == WizardStep::Agent {
                    w.move_selection(true);
                } else if !w.choices.is_empty() {
                    w.selected = (w.selected + 1).min(w.choices.len() - 1);
                }
                UiAction::None
            }
            KeyCode::Backspace if w.step == WizardStep::Dir && alt => {
                w.dir.delete_word_before();
                UiAction::None
            }
            KeyCode::Backspace if w.step == WizardStep::Dir => {
                w.dir.backspace();
                UiAction::None
            }
            KeyCode::Delete if w.step == WizardStep::Dir => {
                w.dir.delete();
                UiAction::None
            }
            KeyCode::Left if w.step == WizardStep::Dir && (alt || ctrl) => {
                w.dir.move_word_left();
                UiAction::None
            }
            KeyCode::Left if w.step == WizardStep::Dir => {
                w.dir.move_left();
                UiAction::None
            }
            KeyCode::Right if w.step == WizardStep::Dir && (alt || ctrl) => {
                w.dir.move_word_right();
                UiAction::None
            }
            KeyCode::Right if w.step == WizardStep::Dir => {
                w.dir.move_right();
                UiAction::None
            }
            KeyCode::Home if w.step == WizardStep::Dir => {
                w.dir.home();
                UiAction::None
            }
            KeyCode::End if w.step == WizardStep::Dir => {
                w.dir.end();
                UiAction::None
            }
            KeyCode::Char('a') if w.step == WizardStep::Dir && ctrl => {
                w.dir.home();
                UiAction::None
            }
            KeyCode::Char('e') if w.step == WizardStep::Dir && ctrl => {
                w.dir.end();
                UiAction::None
            }
            KeyCode::Char('u') if w.step == WizardStep::Dir && ctrl => {
                w.dir.delete_to_line_start();
                UiAction::None
            }
            KeyCode::Char('k') if w.step == WizardStep::Dir && ctrl => {
                w.dir.delete_to_line_end();
                UiAction::None
            }
            KeyCode::Char('w') if w.step == WizardStep::Dir && ctrl => {
                w.dir.delete_word_before();
                UiAction::None
            }
            KeyCode::Char(c) if w.step == WizardStep::Dir && !ctrl && !alt => {
                w.dir.insert_char(c);
                UiAction::None
            }
            KeyCode::Enter if !w.loading => match w.step {
                WizardStep::Agent => match w.choices.get(w.selected).cloned() {
                    Some((_, name)) => {
                        if w.available.get(&name) == Some(&false) {
                            w.error = Some(format!("agent '{name}' is not installed"));
                            UiAction::None
                        } else {
                            w.agent = Some(name.clone());
                            w.capabilities = w.agent_caps.get(&name).copied().unwrap_or_default();
                            w.error = None;
                            w.choices.clear();
                            if !w.capabilities.providers {
                                // No provider catalog: skip straight to the directory.
                                w.step = WizardStep::Dir;
                                w.selected = 0;
                                UiAction::None
                            } else {
                                w.loading = true;
                                UiAction::WizardLoadProviders
                            }
                        }
                    }
                    None => UiAction::None,
                },
                WizardStep::Provider => match w.choices.get(w.selected).cloned() {
                    Some((_, provider_id)) => {
                        w.provider = Some(provider_id.clone());
                        w.selected = 0;
                        if !w.capabilities.model_selection {
                            // No model selection: keep the agent default and go on.
                            w.choices.clear();
                            w.step = WizardStep::Dir;
                            UiAction::None
                        } else {
                            w.step = WizardStep::Model;
                            w.choices = w
                                .providers
                                .iter()
                                .find(|p| p.id == provider_id)
                                .map(|p| {
                                    p.models
                                        .iter()
                                        .map(|m| (m.name.clone(), m.id.clone()))
                                        .collect()
                                })
                                .unwrap_or_default();
                            UiAction::None
                        }
                    }
                    None => UiAction::None,
                },
                WizardStep::Model => match w.choices.get(w.selected).cloned() {
                    Some((_, model_id)) => {
                        w.model = Some(model_id);
                        w.step = WizardStep::Dir;
                        UiAction::None
                    }
                    None => UiAction::None,
                },
                WizardStep::Dir => {
                    let cwd = if w.dir.value().trim().is_empty() {
                        None
                    } else {
                        Some(w.dir.value().trim().to_string())
                    };
                    UiAction::WizardStart {
                        agent: w.agent.clone().unwrap_or_default(),
                        provider: w.provider.clone(),
                        model: w.model.clone(),
                        cwd,
                    }
                }
            },
            _ => UiAction::None,
        };

        // A completed wizard closes; otherwise keep it open.
        if !matches!(action, UiAction::WizardStart { .. }) {
            self.popup = popup;
        }
        action
    }
}
