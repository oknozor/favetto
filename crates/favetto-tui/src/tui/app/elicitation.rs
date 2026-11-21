//! Editable state for an ACP v1 form elicitation.
//!
//! The form is edited field by field, then shown once in a review stage before
//! submission. Accepted values live only in this popup and the
//! `agents.reply_elicitation` request: they are never written to logs, the
//! prompt, the feed or the transcript.

use std::collections::BTreeMap;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use favetto_core::elicitation::{
    ElicitationField, ElicitationFieldKind, ElicitationReply, ElicitationRequest,
    ElicitationValidationError, ElicitationValue,
};

use super::TextBuffer;

/// Which half of the form owns the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElicitationStage {
    Editing,
    Review,
}

/// One field's mutable draft.
#[derive(Debug, Clone, PartialEq)]
pub enum ElicitationDraft {
    Text(TextBuffer),
    Boolean(bool),
    Enum { selected: usize },
}

/// The Ctrl+R form popup for one pending elicitation.
#[derive(Debug, Clone)]
pub struct ElicitationPrompt {
    pub session_id: String,
    pub request: ElicitationRequest,
    pub drafts: Vec<ElicitationDraft>,
    pub current: usize,
    pub stage: ElicitationStage,
    /// Selected review action: 0 submit, 1 decline, 2 cancel.
    pub review_selected: usize,
    pub error: Option<String>,
}

impl ElicitationPrompt {
    pub fn new(session_id: String, request: ElicitationRequest) -> Self {
        let drafts = request.form.fields.iter().map(draft_for).collect();
        Self {
            session_id,
            request,
            drafts,
            current: 0,
            stage: ElicitationStage::Editing,
            review_selected: 0,
            error: None,
        }
    }

    pub fn fields(&self) -> &[ElicitationField] {
        &self.request.form.fields
    }

    pub fn current_field(&self) -> Option<&ElicitationField> {
        self.fields().get(self.current)
    }

    pub fn move_next(&mut self) {
        if !self.fields().is_empty() {
            self.current = (self.current + 1).min(self.fields().len() - 1);
        }
    }

    pub fn move_prev(&mut self) {
        self.current = self.current.saturating_sub(1);
    }

    /// Parse every draft into the value map, reporting the first field that
    /// cannot be represented.
    pub fn values(&self) -> Result<BTreeMap<String, ElicitationValue>, (usize, String)> {
        let mut values = BTreeMap::new();
        for (index, (field, draft)) in self.fields().iter().zip(&self.drafts).enumerate() {
            let value = match draft {
                ElicitationDraft::Boolean(value) => Some(ElicitationValue::Boolean(*value)),
                ElicitationDraft::Enum { selected } => {
                    let ElicitationFieldKind::Enum { options, .. } = &field.kind else {
                        return Err((index, "the form field type changed".to_string()));
                    };
                    options
                        .get(*selected)
                        .map(|option| ElicitationValue::String(option.value.clone()))
                }
                ElicitationDraft::Text(buffer) => match &field.kind {
                    ElicitationFieldKind::Integer { .. } => {
                        if buffer.value().trim().is_empty() {
                            None
                        } else {
                            match buffer.value().trim().parse::<i64>() {
                                Ok(value) => Some(ElicitationValue::Integer(value)),
                                Err(_) => {
                                    return Err((
                                        index,
                                        format!("'{}' must be a whole number", field.name),
                                    ))
                                }
                            }
                        }
                    }
                    ElicitationFieldKind::Number { .. } => {
                        if buffer.value().trim().is_empty() {
                            None
                        } else {
                            match buffer.value().trim().parse::<f64>() {
                                Ok(value) => Some(ElicitationValue::Number(value)),
                                Err(_) => {
                                    return Err((
                                        index,
                                        format!("'{}' must be a number", field.name),
                                    ))
                                }
                            }
                        }
                    }
                    _ => {
                        if field.required || !buffer.value().is_empty() {
                            Some(ElicitationValue::String(buffer.value().to_string()))
                        } else {
                            None
                        }
                    }
                },
            };
            if let Some(value) = value {
                values.insert(field.name.clone(), value);
            }
        }
        Ok(values)
    }

    /// Validate the current drafts and enter the review stage.
    pub fn review(&mut self) -> bool {
        match self.values() {
            Ok(values) => match self.request.form.validate(&values) {
                Ok(()) => {
                    self.error = None;
                    self.stage = ElicitationStage::Review;
                    self.review_selected = 0;
                    true
                }
                Err(error) => {
                    self.focus_error(&error);
                    false
                }
            },
            Err((index, message)) => {
                self.current = index;
                self.stage = ElicitationStage::Editing;
                self.error = Some(message);
                false
            }
        }
    }

    /// Build an accepted reply from validated drafts.
    pub fn accept_reply(&self) -> Result<ElicitationReply, String> {
        let values = self.values().map_err(|(_, message)| message)?;
        self.request
            .form
            .validate(&values)
            .map_err(|error| error.to_string())?;
        Ok(ElicitationReply::Accept { values })
    }

    pub fn cancel_reply(&self) -> ElicitationReply {
        ElicitationReply::Cancel
    }

    fn focus_error(&mut self, error: &ElicitationValidationError) {
        if let Some(name) = &error.field {
            if let Some(index) = self.fields().iter().position(|field| &field.name == name) {
                self.current = index;
            }
        }
        self.stage = ElicitationStage::Editing;
        self.error = Some(error.to_string());
    }

    /// Edit-stage keys. Returns true when the caller should leave the editing
    /// stage (validation passed and review opened, or Esc cancelled).
    pub fn edit_key(&mut self, key: KeyEvent) -> EditOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Tab | KeyCode::Down => {
                self.move_next();
                self.error = None;
                EditOutcome::None
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.move_prev();
                self.error = None;
                EditOutcome::None
            }
            KeyCode::Enter => {
                if self.review() {
                    EditOutcome::Review
                } else {
                    EditOutcome::None
                }
            }
            KeyCode::Esc => EditOutcome::Cancel,
            KeyCode::Left | KeyCode::Right if !ctrl && !alt => {
                if matches!(
                    self.current_kind(),
                    Some(ElicitationFieldKind::Boolean { .. })
                ) {
                    if let Some(ElicitationDraft::Boolean(value)) =
                        self.drafts.get_mut(self.current)
                    {
                        *value = !*value;
                    }
                } else if matches!(self.current_kind(), Some(ElicitationFieldKind::Enum { .. })) {
                    self.cycle_enum(if key.code == KeyCode::Left { -1 } else { 1 });
                } else if let Some(ElicitationDraft::Text(buffer)) =
                    self.drafts.get_mut(self.current)
                {
                    if key.code == KeyCode::Left {
                        buffer.move_left();
                    } else {
                        buffer.move_right();
                    }
                }
                EditOutcome::None
            }
            KeyCode::Char(' ') if !ctrl && !alt => {
                match self.current_kind() {
                    Some(ElicitationFieldKind::Boolean { .. }) => {
                        if let Some(ElicitationDraft::Boolean(value)) =
                            self.drafts.get_mut(self.current)
                        {
                            *value = !*value;
                        }
                    }
                    Some(ElicitationFieldKind::Enum { .. }) => self.cycle_enum(1),
                    _ => self.insert_char(' '),
                }
                EditOutcome::None
            }
            KeyCode::Backspace => {
                if let Some(ElicitationDraft::Text(buffer)) = self.drafts.get_mut(self.current) {
                    buffer.backspace();
                }
                self.error = None;
                EditOutcome::None
            }
            KeyCode::Delete => {
                if let Some(ElicitationDraft::Text(buffer)) = self.drafts.get_mut(self.current) {
                    buffer.delete();
                }
                self.error = None;
                EditOutcome::None
            }
            KeyCode::Home => {
                if let Some(ElicitationDraft::Text(buffer)) = self.drafts.get_mut(self.current) {
                    buffer.home();
                }
                EditOutcome::None
            }
            KeyCode::End => {
                if let Some(ElicitationDraft::Text(buffer)) = self.drafts.get_mut(self.current) {
                    buffer.end();
                }
                EditOutcome::None
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                self.insert_char(c);
                EditOutcome::None
            }
            _ => EditOutcome::None,
        }
    }

    /// Review-stage keys: `Enter` submits, `d` declines, `e` edits, `Esc`
    /// cancels. Left/Right select the explicit action.
    pub fn review_key(&mut self, key: KeyEvent) -> ReviewOutcome {
        match key.code {
            KeyCode::Enter => match self.review_selected {
                0 => match self.accept_reply() {
                    Ok(reply) => ReviewOutcome::Reply(reply),
                    Err(message) => {
                        self.stage = ElicitationStage::Editing;
                        self.error = Some(message);
                        ReviewOutcome::None
                    }
                },
                1 => ReviewOutcome::Reply(ElicitationReply::Decline),
                _ => ReviewOutcome::Reply(ElicitationReply::Cancel),
            },
            KeyCode::Char('d') | KeyCode::Char('D') => {
                ReviewOutcome::Reply(ElicitationReply::Decline)
            }
            KeyCode::Char('e') | KeyCode::Char('E') => {
                self.stage = ElicitationStage::Editing;
                ReviewOutcome::None
            }
            KeyCode::Esc => ReviewOutcome::Reply(ElicitationReply::Cancel),
            KeyCode::Left => {
                self.review_selected = self.review_selected.saturating_sub(1);
                ReviewOutcome::None
            }
            KeyCode::Right => {
                self.review_selected = (self.review_selected + 1).min(2);
                ReviewOutcome::None
            }
            _ => ReviewOutcome::None,
        }
    }

    fn current_kind(&self) -> Option<&ElicitationFieldKind> {
        self.current_field().map(|field| &field.kind)
    }

    fn cycle_enum(&mut self, direction: i32) {
        let Some(ElicitationFieldKind::Enum { options, .. }) = self.current_kind() else {
            return;
        };
        let len = options.len();
        if len == 0 {
            return;
        }
        let Some(ElicitationDraft::Enum { selected }) = self.drafts.get_mut(self.current) else {
            return;
        };
        *selected = if direction < 0 {
            (*selected + len - 1) % len
        } else {
            (*selected + 1) % len
        };
    }

    fn insert_char(&mut self, c: char) {
        if let Some(ElicitationDraft::Text(buffer)) = self.drafts.get_mut(self.current) {
            buffer.insert_char(c);
            self.error = None;
        }
    }

    /// The display label for one field.
    pub fn field_label(field: &ElicitationField) -> &str {
        field.title.as_deref().unwrap_or(&field.name)
    }

    /// The current draft rendered for review.
    pub fn display_value(&self, index: usize) -> String {
        match (self.fields().get(index), self.drafts.get(index)) {
            (_, Some(ElicitationDraft::Boolean(value))) => {
                if *value { "yes" } else { "no" }.to_string()
            }
            (Some(field), Some(ElicitationDraft::Enum { selected })) => {
                let ElicitationFieldKind::Enum { options, .. } = &field.kind else {
                    return String::new();
                };
                options
                    .get(*selected)
                    .map(|option| option.title.clone())
                    .unwrap_or_default()
            }
            (_, Some(ElicitationDraft::Text(buffer))) => {
                let value = buffer.value();
                if value.is_empty() {
                    "—".to_string()
                } else {
                    value.to_string()
                }
            }
            _ => String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditOutcome {
    None,
    Review,
    Cancel,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReviewOutcome {
    None,
    Reply(ElicitationReply),
}

fn draft_for(field: &ElicitationField) -> ElicitationDraft {
    match &field.kind {
        ElicitationFieldKind::String { default, .. } => {
            ElicitationDraft::Text(TextBuffer::new(default.clone().unwrap_or_default()))
        }
        ElicitationFieldKind::Integer { default, .. } => ElicitationDraft::Text(TextBuffer::new(
            default.map(|value| value.to_string()).unwrap_or_default(),
        )),
        ElicitationFieldKind::Number { default, .. } => ElicitationDraft::Text(TextBuffer::new(
            default.map(|value| value.to_string()).unwrap_or_default(),
        )),
        ElicitationFieldKind::Boolean { default } => {
            ElicitationDraft::Boolean(default.unwrap_or(false))
        }
        ElicitationFieldKind::Enum { options, default } => {
            let selected = default
                .as_ref()
                .and_then(|value| options.iter().position(|option| &option.value == value))
                .unwrap_or(0);
            ElicitationDraft::Enum { selected }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use favetto_core::elicitation::{ElicitationForm, ElicitationOption, ElicitationStringFormat};

    fn field(name: &str, required: bool, kind: ElicitationFieldKind) -> ElicitationField {
        ElicitationField {
            name: name.to_string(),
            title: Some(name.to_ascii_uppercase()),
            description: None,
            required,
            kind,
        }
    }

    fn request() -> ElicitationRequest {
        ElicitationRequest {
            id: "acp-elic-1".to_string(),
            agent: "codex".to_string(),
            tool_call_id: None,
            message: "Configure the deployment".to_string(),
            form: ElicitationForm {
                title: Some("Deploy".to_string()),
                description: Some("Review before sending".to_string()),
                fields: vec![
                    field(
                        "name",
                        true,
                        ElicitationFieldKind::String {
                            default: Some("api".to_string()),
                            min_length: Some(2),
                            max_length: Some(40),
                            format: None,
                        },
                    ),
                    field(
                        "region",
                        false,
                        ElicitationFieldKind::Enum {
                            options: vec![
                                ElicitationOption {
                                    value: "eu".to_string(),
                                    title: "Europe".to_string(),
                                    description: None,
                                },
                                ElicitationOption {
                                    value: "us".to_string(),
                                    title: "United States".to_string(),
                                    description: None,
                                },
                            ],
                            default: Some("eu".to_string()),
                        },
                    ),
                    field(
                        "replicas",
                        false,
                        ElicitationFieldKind::Integer {
                            default: Some(2),
                            minimum: Some(1),
                            maximum: Some(9),
                        },
                    ),
                    field(
                        "ratio",
                        false,
                        ElicitationFieldKind::Number {
                            default: Some(0.5),
                            minimum: Some(0.0),
                            maximum: Some(1.0),
                        },
                    ),
                    field(
                        "tls",
                        false,
                        ElicitationFieldKind::Boolean {
                            default: Some(true),
                        },
                    ),
                ],
            },
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn defaults_populate_the_form_and_review_builds_typed_values() {
        let prompt = ElicitationPrompt::new("s1".to_string(), request());
        assert_eq!(prompt.drafts.len(), 5);
        assert_eq!(prompt.display_value(0), "api");
        assert_eq!(prompt.display_value(1), "Europe");
        assert_eq!(prompt.display_value(2), "2");
        assert_eq!(prompt.display_value(3), "0.5");
        assert_eq!(prompt.display_value(4), "yes");

        let values = prompt.values().unwrap();
        assert_eq!(
            values.get("name"),
            Some(&ElicitationValue::String("api".to_string()))
        );
        assert_eq!(
            values.get("region"),
            Some(&ElicitationValue::String("eu".to_string()))
        );
        assert_eq!(values.get("replicas"), Some(&ElicitationValue::Integer(2)));
        assert_eq!(values.get("ratio"), Some(&ElicitationValue::Number(0.5)));
        assert_eq!(values.get("tls"), Some(&ElicitationValue::Boolean(true)));
    }

    #[test]
    fn navigation_editing_enum_cycling_and_boolean_toggle() {
        let mut prompt = ElicitationPrompt::new("s1".to_string(), request());
        assert_eq!(prompt.edit_key(key(KeyCode::Tab)), EditOutcome::None);
        assert_eq!(prompt.current, 1);
        assert_eq!(prompt.edit_key(key(KeyCode::Right)), EditOutcome::None);
        assert_eq!(prompt.display_value(1), "United States");
        assert_eq!(prompt.edit_key(key(KeyCode::BackTab)), EditOutcome::None);
        assert_eq!(prompt.current, 0);
        assert_eq!(prompt.edit_key(key(KeyCode::Backspace)), EditOutcome::None);
        assert_eq!(prompt.display_value(0), "ap");
        prompt.edit_key(key(KeyCode::Char('i')));
        assert_eq!(prompt.display_value(0), "api");

        while prompt.current < 4 {
            prompt.edit_key(key(KeyCode::Tab));
        }
        prompt.edit_key(key(KeyCode::Char(' ')));
        assert_eq!(prompt.display_value(4), "no");
        prompt.edit_key(key(KeyCode::Left));
        assert_eq!(prompt.display_value(4), "yes");
    }

    #[test]
    fn review_shows_values_and_edit_returns_without_losing_them() {
        let mut prompt = ElicitationPrompt::new("s1".to_string(), request());
        assert_eq!(prompt.edit_key(key(KeyCode::Enter)), EditOutcome::Review);
        assert_eq!(prompt.stage, ElicitationStage::Review);
        assert_eq!(
            prompt.review_key(key(KeyCode::Char('e'))),
            ReviewOutcome::None
        );
        assert_eq!(prompt.stage, ElicitationStage::Editing);
        assert_eq!(prompt.display_value(0), "api");
        assert_eq!(prompt.edit_key(key(KeyCode::Esc)), EditOutcome::Cancel);
    }

    #[test]
    fn review_actions_are_distinct() {
        let mut prompt = ElicitationPrompt::new("s1".to_string(), request());
        prompt.stage = ElicitationStage::Review;
        assert_eq!(
            prompt.review_key(key(KeyCode::Char('d'))),
            ReviewOutcome::Reply(ElicitationReply::Decline)
        );
        assert_eq!(
            prompt.review_key(key(KeyCode::Esc)),
            ReviewOutcome::Reply(ElicitationReply::Cancel)
        );
        assert!(matches!(
            prompt.review_key(key(KeyCode::Enter)),
            ReviewOutcome::Reply(ElicitationReply::Accept { .. })
        ));
    }

    #[test]
    fn invalid_number_stays_in_editing_and_focuses_the_field() {
        let mut prompt = ElicitationPrompt::new("s1".to_string(), request());
        prompt.current = 2;
        for _ in 0..4 {
            prompt.edit_key(key(KeyCode::Backspace));
        }
        for c in "lots".chars() {
            prompt.edit_key(key(KeyCode::Char(c)));
        }
        assert_eq!(prompt.edit_key(key(KeyCode::Enter)), EditOutcome::None);
        assert_eq!(prompt.stage, ElicitationStage::Editing);
        assert_eq!(prompt.current, 2);
        assert!(prompt.error.is_some());
    }

    #[test]
    fn missing_required_and_enum_membership_are_reported() {
        let mut prompt = ElicitationPrompt::new("s1".to_string(), request());
        for _ in 0..3 {
            prompt.edit_key(key(KeyCode::Backspace));
        }
        assert!(!prompt.review());
        assert_eq!(prompt.current, 0);
        assert!(prompt.error.unwrap().contains("name"));

        let mut prompt = ElicitationPrompt::new("s1".to_string(), request());
        prompt.current = 1;
        if let ElicitationDraft::Enum { selected } = &mut prompt.drafts[1] {
            *selected = 0;
        }
        // A forced out-of-range index cannot be produced by the UI, but the
        // shared validator still rejects an unknown enum value.
        let mut values = prompt.values().unwrap();
        values.insert(
            "region".to_string(),
            ElicitationValue::String("unknown".to_string()),
        );
        let error = prompt.request.form.validate(&values).unwrap_err();
        assert_eq!(error.field.as_deref(), Some("region"));
    }

    #[test]
    fn formats_are_validated_by_the_shared_validator() {
        let prompt = ElicitationPrompt::new(
            "s1".to_string(),
            ElicitationRequest {
                id: "e".to_string(),
                agent: "codex".to_string(),
                tool_call_id: None,
                message: "email".to_string(),
                form: ElicitationForm {
                    title: None,
                    description: None,
                    fields: vec![field(
                        "email",
                        true,
                        ElicitationFieldKind::String {
                            default: None,
                            min_length: None,
                            max_length: None,
                            format: Some(ElicitationStringFormat::Email),
                        },
                    )],
                },
            },
        );
        let mut values = BTreeMap::new();
        values.insert(
            "email".to_string(),
            ElicitationValue::String("not-an-email".to_string()),
        );
        assert!(prompt.request.form.validate(&values).is_err());
    }
}
