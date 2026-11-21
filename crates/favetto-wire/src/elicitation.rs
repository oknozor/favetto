//! ACP v1 form elicitation types shared by the daemon and clients.
//!
//! The wire form is deliberately restricted to the primitive controls both
//! desktop clients can render: string, single-select enum, integer, number and
//! boolean. Multi-select arrays, string `pattern` constraints and URL mode are
//! unsupported and rejected explicitly by the daemon.
//!
//! Submitted values are sensitive user input. [`ElicitationReply`] has a
//! hand-written [`std::fmt::Debug`] that never prints the accepted values, so an
//! accidental `tracing`/`dbg!`/assertion cannot leak a form response.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Maximum number of fields in one elicitation form.
pub const MAX_ELICITATION_FIELDS: usize = 32;
/// Maximum number of options in one enum field.
pub const MAX_ELICITATION_OPTIONS: usize = 128;
/// Maximum length of a form title, description or field label.
pub const MAX_ELICITATION_LABEL_LEN: usize = 4096;

/// One pending form request, carrying the ACP request correlation and the
/// configured agent that asked for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ElicitationRequest {
    /// Favetto correlation id (for example `acp-elic-7`).
    pub id: String,
    /// Configured `[agents.*]` name.
    pub agent: String,
    /// The ACP session scope's tool call id, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// The agent's human-readable message.
    pub message: String,
    /// The renderable form.
    pub form: ElicitationForm,
}

/// A form the user can edit, review, accept, decline or cancel.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ElicitationForm {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Fields sorted by name for deterministic rendering.
    #[serde(default)]
    pub fields: Vec<ElicitationField>,
}

/// One editable field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ElicitationField {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
    pub kind: ElicitationFieldKind,
}

/// The supported field controls.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ElicitationFieldKind {
    String {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_length: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_length: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<ElicitationStringFormat>,
    },
    Enum {
        options: Vec<ElicitationOption>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
    },
    Integer {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        minimum: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        maximum: Option<i64>,
    },
    Number {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        minimum: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        maximum: Option<f64>,
    },
    Boolean {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<bool>,
    },
}

/// A single-select option.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ElicitationOption {
    pub value: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A supported string format constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ElicitationStringFormat {
    Email,
    Uri,
    Date,
    DateTime,
}

/// One accepted value. The explicit tag keeps integer and number distinct on
/// the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ElicitationValue {
    String(String),
    Integer(i64),
    Number(f64),
    Boolean(bool),
}

/// The user's answer to a form request.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ElicitationReply {
    Accept {
        values: BTreeMap<String, ElicitationValue>,
    },
    Decline,
    Cancel,
}

impl fmt::Debug for ElicitationReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ElicitationReply::Accept { values } => f
                .debug_struct("Accept")
                .field(
                    "values",
                    &format_args!("<{} value(s) redacted>", values.len()),
                )
                .finish(),
            ElicitationReply::Decline => f.write_str("Decline"),
            ElicitationReply::Cancel => f.write_str("Cancel"),
        }
    }
}

/// The terminal outcome of one elicitation request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ElicitationOutcome {
    Accepted,
    Declined,
    Cancelled,
}

/// A validation failure. Messages name the field and constraint but never echo
/// the submitted value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[error("{message}")]
pub struct ElicitationValidationError {
    pub field: Option<String>,
    pub message: String,
}

impl ElicitationValidationError {
    pub fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: Some(field.into()),
            message: message.into(),
        }
    }

    pub fn form(message: impl Into<String>) -> Self {
        Self {
            field: None,
            message: message.into(),
        }
    }
}

impl ElicitationForm {
    /// Validate a value map against every field and constraint.
    ///
    /// This is the single validator shared by the TUI, GPUI and daemon. It is
    /// deterministic and reports the first failure.
    pub fn validate(
        &self,
        values: &BTreeMap<String, ElicitationValue>,
    ) -> Result<(), ElicitationValidationError> {
        for name in values.keys() {
            if !self.fields.iter().any(|field| field.name == *name) {
                return Err(ElicitationValidationError::new(
                    name.clone(),
                    format!("unknown form field '{name}'"),
                ));
            }
        }
        for field in &self.fields {
            match values.get(&field.name) {
                Some(value) => field.validate_value(value)?,
                None if field.required => {
                    return Err(ElicitationValidationError::new(
                        field.name.clone(),
                        format!("field '{}' is required", field.name),
                    ));
                }
                None => {}
            }
        }
        Ok(())
    }

    /// Whether the form can be rendered by both supported clients.
    pub fn is_supported(&self) -> bool {
        self.fields.len() <= MAX_ELICITATION_FIELDS
            && self.fields.iter().all(ElicitationField::is_supported)
    }
}

impl ElicitationField {
    fn is_supported(&self) -> bool {
        match &self.kind {
            ElicitationFieldKind::Enum { options, .. } => {
                !options.is_empty() && options.len() <= MAX_ELICITATION_OPTIONS
            }
            _ => true,
        }
    }

    pub fn validate_value(
        &self,
        value: &ElicitationValue,
    ) -> Result<(), ElicitationValidationError> {
        match (&self.kind, value) {
            (
                ElicitationFieldKind::String {
                    min_length,
                    max_length,
                    format,
                    ..
                },
                ElicitationValue::String(value),
            ) => {
                let len = value.chars().count();
                if let Some(min) = min_length {
                    if len < usize::try_from(*min).unwrap_or(usize::MAX) {
                        return Err(ElicitationValidationError::new(
                            self.name.clone(),
                            format!("field '{}' is shorter than {} characters", self.name, min),
                        ));
                    }
                }
                if let Some(max) = max_length {
                    if len > usize::try_from(*max).unwrap_or(usize::MAX) {
                        return Err(ElicitationValidationError::new(
                            self.name.clone(),
                            format!("field '{}' is longer than {} characters", self.name, max),
                        ));
                    }
                }
                if let Some(format) = format {
                    let valid = match format {
                        ElicitationStringFormat::Email => valid_email(value),
                        ElicitationStringFormat::Uri => url::Url::parse(value).is_ok(),
                        ElicitationStringFormat::Date => {
                            chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok()
                        }
                        ElicitationStringFormat::DateTime => {
                            chrono::DateTime::parse_from_rfc3339(value).is_ok()
                        }
                    };
                    if !valid {
                        let label = match format {
                            ElicitationStringFormat::Email => "an email address",
                            ElicitationStringFormat::Uri => "a URI",
                            ElicitationStringFormat::Date => "a date (YYYY-MM-DD)",
                            ElicitationStringFormat::DateTime => "an RFC 3339 date-time",
                        };
                        return Err(ElicitationValidationError::new(
                            self.name.clone(),
                            format!("field '{}' must be {}", self.name, label),
                        ));
                    }
                }
                Ok(())
            }
            (ElicitationFieldKind::Enum { options, .. }, ElicitationValue::String(value)) => {
                if options.iter().any(|option| &option.value == value) {
                    Ok(())
                } else {
                    Err(ElicitationValidationError::new(
                        self.name.clone(),
                        format!("field '{}' is not one of the offered options", self.name),
                    ))
                }
            }
            (
                ElicitationFieldKind::Integer {
                    minimum, maximum, ..
                },
                ElicitationValue::Integer(value),
            ) => {
                if minimum.is_some_and(|min| *value < min) {
                    return Err(ElicitationValidationError::new(
                        self.name.clone(),
                        format!("field '{}' is below its minimum", self.name),
                    ));
                }
                if maximum.is_some_and(|max| *value > max) {
                    return Err(ElicitationValidationError::new(
                        self.name.clone(),
                        format!("field '{}' is above its maximum", self.name),
                    ));
                }
                Ok(())
            }
            (
                ElicitationFieldKind::Number {
                    minimum, maximum, ..
                },
                ElicitationValue::Number(value),
            ) => {
                if !value.is_finite() {
                    return Err(ElicitationValidationError::new(
                        self.name.clone(),
                        format!("field '{}' must be a finite number", self.name),
                    ));
                }
                if minimum.is_some_and(|min| *value < min) {
                    return Err(ElicitationValidationError::new(
                        self.name.clone(),
                        format!("field '{}' is below its minimum", self.name),
                    ));
                }
                if maximum.is_some_and(|max| *value > max) {
                    return Err(ElicitationValidationError::new(
                        self.name.clone(),
                        format!("field '{}' is above its maximum", self.name),
                    ));
                }
                Ok(())
            }
            (ElicitationFieldKind::Boolean { .. }, ElicitationValue::Boolean(_)) => Ok(()),
            _ => Err(ElicitationValidationError::new(
                self.name.clone(),
                format!("field '{}' has the wrong value type", self.name),
            )),
        }
    }
}

fn valid_email(value: &str) -> bool {
    if value.chars().any(char::is_whitespace) {
        return false;
    }
    let mut parts = value.split('@');
    let Some(local) = parts.next() else {
        return false;
    };
    let Some(domain) = parts.next() else {
        return false;
    };
    if parts.next().is_some() || local.is_empty() || domain.is_empty() {
        return false;
    }
    domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !local.starts_with('.')
        && !local.ends_with('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string_field(name: &str, required: bool) -> ElicitationField {
        ElicitationField {
            name: name.to_string(),
            title: None,
            description: None,
            required,
            kind: ElicitationFieldKind::String {
                default: None,
                min_length: None,
                max_length: None,
                format: None,
            },
        }
    }

    fn form(fields: Vec<ElicitationField>) -> ElicitationForm {
        ElicitationForm {
            title: Some("Details".to_string()),
            description: Some("Fill this in".to_string()),
            fields,
        }
    }

    #[test]
    fn form_and_reply_round_trip() {
        let form = ElicitationForm {
            title: Some("Profile".to_string()),
            description: None,
            fields: vec![
                ElicitationField {
                    name: "name".to_string(),
                    title: Some("Name".to_string()),
                    description: Some("Your name".to_string()),
                    required: true,
                    kind: ElicitationFieldKind::String {
                        default: Some("Ada".to_string()),
                        min_length: Some(1),
                        max_length: Some(80),
                        format: None,
                    },
                },
                ElicitationField {
                    name: "color".to_string(),
                    title: None,
                    description: None,
                    required: false,
                    kind: ElicitationFieldKind::Enum {
                        options: vec![
                            ElicitationOption {
                                value: "red".to_string(),
                                title: "Red".to_string(),
                                description: None,
                            },
                            ElicitationOption {
                                value: "blue".to_string(),
                                title: "Blue".to_string(),
                                description: None,
                            },
                        ],
                        default: Some("red".to_string()),
                    },
                },
                ElicitationField {
                    name: "age".to_string(),
                    title: None,
                    description: None,
                    required: false,
                    kind: ElicitationFieldKind::Integer {
                        default: Some(30),
                        minimum: Some(0),
                        maximum: Some(150),
                    },
                },
                ElicitationField {
                    name: "ratio".to_string(),
                    title: None,
                    description: None,
                    required: false,
                    kind: ElicitationFieldKind::Number {
                        default: Some(1.5),
                        minimum: Some(0.0),
                        maximum: Some(10.0),
                    },
                },
                ElicitationField {
                    name: "ok".to_string(),
                    title: None,
                    description: None,
                    required: false,
                    kind: ElicitationFieldKind::Boolean {
                        default: Some(true),
                    },
                },
            ],
        };
        let json = serde_json::to_value(&form).unwrap();
        let back: ElicitationForm = serde_json::from_value(json).unwrap();
        assert_eq!(back, form);

        let mut values = BTreeMap::new();
        values.insert(
            "name".to_string(),
            ElicitationValue::String("Ada".to_string()),
        );
        values.insert("age".to_string(), ElicitationValue::Integer(36));
        values.insert("ratio".to_string(), ElicitationValue::Number(2.25));
        values.insert("ok".to_string(), ElicitationValue::Boolean(false));
        values.insert(
            "color".to_string(),
            ElicitationValue::String("blue".to_string()),
        );
        let reply = ElicitationReply::Accept { values };
        let json = serde_json::to_value(&reply).unwrap();
        let back: ElicitationReply = serde_json::from_value(json).unwrap();
        assert_eq!(back, reply);

        for reply in [ElicitationReply::Decline, ElicitationReply::Cancel] {
            let json = serde_json::to_value(&reply).unwrap();
            let back: ElicitationReply = serde_json::from_value(json).unwrap();
            assert_eq!(back, reply);
        }
    }

    #[test]
    fn debug_redacts_accepted_values() {
        let mut values = BTreeMap::new();
        values.insert(
            "secret".to_string(),
            ElicitationValue::String("super-secret-value".to_string()),
        );
        let debug = format!("{:?}", ElicitationReply::Accept { values });
        assert!(!debug.contains("super-secret-value"));
        assert!(debug.contains("redacted"));
    }

    #[test]
    fn validation_accepts_supported_kinds_and_defaults() {
        let form = form(vec![
            ElicitationField {
                name: "email".to_string(),
                title: None,
                description: None,
                required: true,
                kind: ElicitationFieldKind::String {
                    default: None,
                    min_length: Some(3),
                    max_length: Some(100),
                    format: Some(ElicitationStringFormat::Email),
                },
            },
            ElicitationField {
                name: "site".to_string(),
                title: None,
                description: None,
                required: false,
                kind: ElicitationFieldKind::String {
                    default: None,
                    min_length: None,
                    max_length: None,
                    format: Some(ElicitationStringFormat::Uri),
                },
            },
            ElicitationField {
                name: "day".to_string(),
                title: None,
                description: None,
                required: false,
                kind: ElicitationFieldKind::String {
                    default: None,
                    min_length: None,
                    max_length: None,
                    format: Some(ElicitationStringFormat::Date),
                },
            },
            ElicitationField {
                name: "when".to_string(),
                title: None,
                description: None,
                required: false,
                kind: ElicitationFieldKind::String {
                    default: None,
                    min_length: None,
                    max_length: None,
                    format: Some(ElicitationStringFormat::DateTime),
                },
            },
            ElicitationField {
                name: "choice".to_string(),
                title: None,
                description: None,
                required: false,
                kind: ElicitationFieldKind::Enum {
                    options: vec![ElicitationOption {
                        value: "a".to_string(),
                        title: "A".to_string(),
                        description: None,
                    }],
                    default: Some("a".to_string()),
                },
            },
            ElicitationField {
                name: "count".to_string(),
                title: None,
                description: None,
                required: false,
                kind: ElicitationFieldKind::Integer {
                    default: Some(1),
                    minimum: Some(0),
                    maximum: Some(10),
                },
            },
            ElicitationField {
                name: "ratio".to_string(),
                title: None,
                description: None,
                required: false,
                kind: ElicitationFieldKind::Number {
                    default: Some(0.5),
                    minimum: Some(0.0),
                    maximum: Some(1.0),
                },
            },
            ElicitationField {
                name: "enabled".to_string(),
                title: None,
                description: None,
                required: false,
                kind: ElicitationFieldKind::Boolean {
                    default: Some(true),
                },
            },
        ]);

        let mut values = BTreeMap::new();
        values.insert(
            "email".to_string(),
            ElicitationValue::String("user@example.com".to_string()),
        );
        values.insert(
            "site".to_string(),
            ElicitationValue::String("https://example.com/a".to_string()),
        );
        values.insert(
            "day".to_string(),
            ElicitationValue::String("2026-11-21".to_string()),
        );
        values.insert(
            "when".to_string(),
            ElicitationValue::String("2026-11-21T10:00:00Z".to_string()),
        );
        values.insert(
            "choice".to_string(),
            ElicitationValue::String("a".to_string()),
        );
        values.insert("count".to_string(), ElicitationValue::Integer(2));
        values.insert("ratio".to_string(), ElicitationValue::Number(0.75));
        values.insert("enabled".to_string(), ElicitationValue::Boolean(false));
        form.validate(&values).unwrap();
    }

    #[test]
    fn validation_rejects_bad_values_without_echoing_them() {
        let form = form(vec![
            ElicitationField {
                name: "email".to_string(),
                title: None,
                description: None,
                required: true,
                kind: ElicitationFieldKind::String {
                    default: None,
                    min_length: None,
                    max_length: None,
                    format: Some(ElicitationStringFormat::Email),
                },
            },
            ElicitationField {
                name: "count".to_string(),
                title: None,
                description: None,
                required: false,
                kind: ElicitationFieldKind::Integer {
                    default: None,
                    minimum: Some(0),
                    maximum: Some(10),
                },
            },
            ElicitationField {
                name: "ratio".to_string(),
                title: None,
                description: None,
                required: false,
                kind: ElicitationFieldKind::Number {
                    default: None,
                    minimum: None,
                    maximum: None,
                },
            },
            ElicitationField {
                name: "choice".to_string(),
                title: None,
                description: None,
                required: false,
                kind: ElicitationFieldKind::Enum {
                    options: vec![ElicitationOption {
                        value: "a".to_string(),
                        title: "A".to_string(),
                        description: None,
                    }],
                    default: None,
                },
            },
        ]);

        let mut missing = BTreeMap::new();
        let error = form.validate(&missing).unwrap_err();
        assert_eq!(error.field.as_deref(), Some("email"));

        missing.insert(
            "email".to_string(),
            ElicitationValue::String("not-an-email-secret".to_string()),
        );
        let error = form.validate(&missing).unwrap_err();
        assert_eq!(error.field.as_deref(), Some("email"));
        assert!(!error.to_string().contains("not-an-email-secret"));

        let mut unknown = missing.clone();
        unknown.insert(
            "extra".to_string(),
            ElicitationValue::String("hidden-extra".to_string()),
        );
        let error = form.validate(&unknown).unwrap_err();
        assert_eq!(error.field.as_deref(), Some("extra"));

        let mut values = BTreeMap::new();
        values.insert(
            "email".to_string(),
            ElicitationValue::String("user@example.com".to_string()),
        );
        values.insert("count".to_string(), ElicitationValue::Integer(99));
        let error = form.validate(&values).unwrap_err();
        assert_eq!(error.field.as_deref(), Some("count"));

        values.insert("count".to_string(), ElicitationValue::Integer(1));
        values.insert("ratio".to_string(), ElicitationValue::Integer(1));
        let error = form.validate(&values).unwrap_err();
        assert_eq!(error.field.as_deref(), Some("ratio"));

        values.insert("ratio".to_string(), ElicitationValue::Number(1.0));
        values.insert(
            "choice".to_string(),
            ElicitationValue::String("z".to_string()),
        );
        let error = form.validate(&values).unwrap_err();
        assert_eq!(error.field.as_deref(), Some("choice"));
    }

    #[test]
    fn optional_blank_values_and_empty_forms_are_valid() {
        let with_note = form(vec![string_field("note", false)]);
        let mut values = BTreeMap::new();
        values.insert("note".to_string(), ElicitationValue::String(String::new()));
        with_note.validate(&values).unwrap();
        with_note.validate(&BTreeMap::new()).unwrap();

        let empty = form(Vec::new());
        empty.validate(&BTreeMap::new()).unwrap();
        assert!(empty.is_supported());
    }

    #[test]
    fn reply_debug_is_redacted_in_all_forms() {
        let mut values = BTreeMap::new();
        values.insert("pin".to_string(), ElicitationValue::Integer(123456));
        let debug = format!("{:?}", ElicitationReply::Accept { values });
        assert!(!debug.contains("123456"));
        assert_eq!(format!("{:?}", ElicitationReply::Decline), "Decline");
        assert_eq!(format!("{:?}", ElicitationReply::Cancel), "Cancel");
    }
}
