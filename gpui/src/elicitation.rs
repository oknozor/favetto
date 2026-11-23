//! Pure state machine for an ACP v1 form elicitation in the GPUI client.
//!
//! Values never leave this state machine except in an `ElicitationReply`; they
//! are not logged or folded into the feed.

use std::collections::BTreeMap;

use crate::text_input::TextInput;
use favetto_wire::elicitation::{
    ElicitationField, ElicitationFieldKind, ElicitationReply, ElicitationRequest,
    ElicitationValidationError, ElicitationValue,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElicitationStage {
    Editing,
    Review,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ElicitationDraft {
    Text(TextInput),
    Boolean(bool),
    Enum { selected: usize },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ElicitationFormState {
    pub session_id: String,
    pub request: ElicitationRequest,
    pub drafts: Vec<ElicitationDraft>,
    pub current: usize,
    pub stage: ElicitationStage,
    pub review_selected: usize,
    pub error: Option<String>,
}

impl ElicitationFormState {
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

    pub fn move_next(&mut self) {
        if !self.fields().is_empty() {
            self.current = (self.current + 1).min(self.fields().len() - 1);
        }
    }

    pub fn move_prev(&mut self) {
        self.current = self.current.saturating_sub(1);
    }

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
                ElicitationDraft::Text(value) => match &field.kind {
                    ElicitationFieldKind::Integer { .. } => {
                        if value.value().trim().is_empty() {
                            None
                        } else {
                            match value.value().trim().parse::<i64>() {
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
                        if value.value().trim().is_empty() {
                            None
                        } else {
                            match value.value().trim().parse::<f64>() {
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
                        if field.required || !value.is_empty() {
                            Some(ElicitationValue::String(value.value().to_string()))
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

    pub fn accept_reply(&self) -> Result<ElicitationReply, String> {
        let values = self.values().map_err(|(_, message)| message)?;
        self.request
            .form
            .validate(&values)
            .map_err(|error| error.to_string())?;
        Ok(ElicitationReply::Accept { values })
    }

    pub fn decline(&mut self) -> ElicitationReply {
        ElicitationReply::Decline
    }

    pub fn cancel(&mut self) -> ElicitationReply {
        ElicitationReply::Cancel
    }

    #[allow(dead_code)] // Exercised by the model's tests; the view edits the buffer.
    pub fn push_char(&mut self, ch: char) {
        if let Some(ElicitationDraft::Text(value)) = self.drafts.get_mut(self.current) {
            value.push_char(ch);
            self.error = None;
        }
    }

    #[allow(dead_code)] // Exercised by the model's tests; the view edits the buffer.
    pub fn backspace(&mut self) {
        if let Some(ElicitationDraft::Text(value)) = self.drafts.get_mut(self.current) {
            value.backspace();
            self.error = None;
        }
    }

    /// The editable buffer for the current field, when it is text-like.
    pub fn text_input(&self) -> Option<&TextInput> {
        self.text_input_at(self.current)
    }

    pub fn text_input_at(&self, index: usize) -> Option<&TextInput> {
        match self.drafts.get(index) {
            Some(ElicitationDraft::Text(value)) => Some(value),
            _ => None,
        }
    }

    pub fn text_input_at_mut(&mut self, index: usize) -> Option<&mut TextInput> {
        match self.drafts.get_mut(index) {
            Some(ElicitationDraft::Text(value)) => Some(value),
            _ => None,
        }
    }

    /// Focus one field (a click on its row or the element's bounds).
    pub fn set_current(&mut self, index: usize) {
        if index < self.drafts.len() {
            self.current = index;
        }
    }

    pub fn after_edit(&mut self) {
        self.error = None;
    }

    pub fn toggle_or_cycle(&mut self, backwards: bool) {
        match self.current_kind() {
            Some(ElicitationFieldKind::Boolean { .. }) => {
                if let Some(ElicitationDraft::Boolean(value)) = self.drafts.get_mut(self.current) {
                    *value = !*value;
                }
            }
            Some(ElicitationFieldKind::Enum { .. }) => self.cycle_enum(backwards),
            _ => {}
        }
    }

    fn cycle_enum(&mut self, backwards: bool) {
        let Some(ElicitationFieldKind::Enum { options, .. }) = self.current_kind() else {
            return;
        };
        let len = options.len();
        if len == 0 {
            return;
        }
        if let Some(ElicitationDraft::Enum { selected }) = self.drafts.get_mut(self.current) {
            *selected = if backwards {
                (*selected + len - 1) % len
            } else {
                (*selected + 1) % len
            };
        }
    }

    fn current_kind(&self) -> Option<&ElicitationFieldKind> {
        self.fields().get(self.current).map(|field| &field.kind)
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

    pub fn display_value(&self, index: usize) -> String {
        match (self.fields().get(index), self.drafts.get(index)) {
            (_, Some(ElicitationDraft::Boolean(value))) => {
                if *value { "Yes" } else { "No" }.to_string()
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
            (_, Some(ElicitationDraft::Text(value))) => {
                if value.is_empty() {
                    "—".to_string()
                } else {
                    value.value().to_string()
                }
            }
            _ => String::new(),
        }
    }

    pub fn field_label(field: &ElicitationField) -> &str {
        field.title.as_deref().unwrap_or(&field.name)
    }
}

fn draft_for(field: &ElicitationField) -> ElicitationDraft {
    match &field.kind {
        ElicitationFieldKind::String { default, .. } => {
            ElicitationDraft::Text(TextInput::new(default.clone().unwrap_or_default()))
        }
        ElicitationFieldKind::Integer { default, .. } => ElicitationDraft::Text(TextInput::new(
            default.map(|value| value.to_string()).unwrap_or_default(),
        )),
        ElicitationFieldKind::Number { default, .. } => ElicitationDraft::Text(TextInput::new(
            default.map(|value| value.to_string()).unwrap_or_default(),
        )),
        ElicitationFieldKind::Boolean { default } => {
            ElicitationDraft::Boolean(default.unwrap_or(false))
        }
        ElicitationFieldKind::Enum { options, default } => ElicitationDraft::Enum {
            selected: default
                .as_ref()
                .and_then(|value| options.iter().position(|option| &option.value == value))
                .unwrap_or(0),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use favetto_wire::elicitation::{ElicitationForm, ElicitationOption, ElicitationStringFormat};

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
            message: "Configure".to_string(),
            form: ElicitationForm {
                title: Some("Deploy".to_string()),
                description: None,
                fields: vec![
                    field(
                        "name",
                        true,
                        ElicitationFieldKind::String {
                            default: Some("api".to_string()),
                            min_length: Some(2),
                            max_length: Some(20),
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
                        "tls",
                        false,
                        ElicitationFieldKind::Boolean {
                            default: Some(true),
                        },
                    ),
                    field(
                        "email",
                        false,
                        ElicitationFieldKind::String {
                            default: None,
                            min_length: None,
                            max_length: None,
                            format: Some(ElicitationStringFormat::Email),
                        },
                    ),
                ],
            },
        }
    }

    #[test]
    fn defaults_and_editing_produce_typed_values() {
        let mut form = ElicitationFormState::new("s1".to_string(), request());
        assert_eq!(form.display_value(0), "api");
        assert_eq!(form.display_value(1), "Europe");
        assert_eq!(form.display_value(2), "2");
        assert_eq!(form.display_value(3), "Yes");
        form.move_next();
        form.toggle_or_cycle(false);
        assert_eq!(form.display_value(1), "United States");
        form.move_next();
        form.backspace();
        form.push_char('5');
        assert_eq!(form.display_value(2), "5");
        form.move_next();
        form.toggle_or_cycle(false);
        assert_eq!(form.display_value(3), "No");

        let values = form.values().unwrap();
        assert_eq!(
            values.get("region"),
            Some(&ElicitationValue::String("us".to_string()))
        );
        assert_eq!(values.get("replicas"), Some(&ElicitationValue::Integer(5)));
        assert_eq!(values.get("tls"), Some(&ElicitationValue::Boolean(false)));
    }

    #[test]
    fn review_requires_validation_and_values_stay_until_edit() {
        let mut form = ElicitationFormState::new("s1".to_string(), request());
        assert!(form.review());
        assert_eq!(form.stage, ElicitationStage::Review);
        form.stage = ElicitationStage::Editing;
        assert_eq!(form.display_value(0), "api");
        assert!(matches!(
            form.accept_reply(),
            Ok(ElicitationReply::Accept { .. })
        ));
        assert_eq!(form.decline(), ElicitationReply::Decline);
        assert_eq!(form.cancel(), ElicitationReply::Cancel);
    }

    #[test]
    fn invalid_values_focus_the_field_and_do_not_leak_the_value() {
        let mut form = ElicitationFormState::new("s1".to_string(), request());
        form.current = 4;
        for c in "not-an-email".chars() {
            form.push_char(c);
        }
        assert!(!form.review());
        assert_eq!(form.stage, ElicitationStage::Editing);
        assert_eq!(form.current, 4);
        let error = form.error.clone().unwrap();
        assert!(error.contains("email"));
        assert!(!error.contains("not-an-email"));

        let mut missing = ElicitationFormState::new("s1".to_string(), request());
        for _ in 0..3 {
            missing.backspace();
        }
        assert!(!missing.review());
        assert_eq!(missing.current, 0);
    }

    #[test]
    fn the_reply_debug_never_prints_values() {
        let form = ElicitationFormState::new("s1".to_string(), request());
        let reply = form.accept_reply().unwrap();
        assert!(!format!("{reply:?}").contains("api"));
    }
}
