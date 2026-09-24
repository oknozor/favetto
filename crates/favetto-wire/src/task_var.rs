//! Manual task input variables, shared by the daemon and TUI.
//!
//! These are the pure value types behind a task's `[[vars]]` front-matter. The
//! Markdown catalog parser lives in `favetto-core`; this module owns the
//! user-facing value types and the raw-string coercion both ends need.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The declared type of a manual input variable. `int`/`bool` values are coerced
/// to JSON numbers/bools before being stored in the task's `input`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum VarType {
    #[default]
    String,
    Int,
    Bool,
}

/// A manual input variable declared in a task's `[[vars]]` front-matter. The
/// collected value becomes `input.<name>` and renders through `{{ input.<name> }}`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TaskVar {
    /// The `input` key. `[a-zA-Z0-9_]+`, unique within the file, never `_prev`.
    pub name: String,
    /// The label/question shown in the TUI prompt popup.
    pub prompt: String,
    /// Optional value pre-filled in the popup.
    #[serde(default)]
    pub default: Option<String>,
    /// When true, submission is blocked while the field is empty.
    #[serde(default)]
    pub required: bool,
    /// When true, `Enter` inserts a newline and the submit chord completes the form.
    #[serde(default)]
    pub multiline: bool,
    /// The value type; `int`/`bool` are parsed before being stored.
    #[serde(default, rename = "type")]
    pub var_type: VarType,
    /// Optional fixed list of options, rendered as a selectable list.
    #[serde(default)]
    pub choices: Option<Vec<String>>,
}

/// Failure to coerce a raw form value into a [`TaskVar`]'s declared type.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum VarCoerceError {
    /// The value is not one of the variable's fixed choices.
    #[error("must be one of: {0}")]
    NotInChoices(String),
    /// The value could not be parsed as an integer.
    #[error("must be an integer")]
    NotAnInteger,
    /// The value could not be parsed as a boolean.
    #[error("must be true or false")]
    NotBoolean,
}

impl TaskVar {
    /// Coerce a raw string entered by the user into the var's JSON representation.
    pub fn coerce(&self, raw: &str) -> Result<serde_json::Value, VarCoerceError> {
        if let Some(choices) = &self.choices {
            if !choices.iter().any(|c| c == raw) {
                return Err(VarCoerceError::NotInChoices(choices.join(", ")));
            }
        }
        match self.var_type {
            VarType::String => Ok(serde_json::Value::String(raw.to_string())),
            VarType::Int => raw
                .trim()
                .parse::<i64>()
                .map(|n| serde_json::Value::Number(n.into()))
                .map_err(|_| VarCoerceError::NotAnInteger),
            VarType::Bool => match raw.trim().to_ascii_lowercase().as_str() {
                "true" | "1" => Ok(serde_json::Value::Bool(true)),
                "false" | "0" => Ok(serde_json::Value::Bool(false)),
                _ => Err(VarCoerceError::NotBoolean),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coerces_typed_values() {
        let var = |var_type, choices| TaskVar {
            name: "v".to_string(),
            prompt: "p".to_string(),
            default: None,
            required: false,
            multiline: false,
            var_type,
            choices,
        };
        assert_eq!(
            var(VarType::String, None).coerce("hi").unwrap(),
            serde_json::json!("hi")
        );
        assert_eq!(
            var(VarType::Int, None).coerce(" 3 ").unwrap(),
            serde_json::json!(3)
        );
        assert!(var(VarType::Int, None).coerce("not an int").is_err());
        assert_eq!(
            var(VarType::Bool, None).coerce("True").unwrap(),
            serde_json::json!(true)
        );
        assert_eq!(
            var(VarType::Bool, None).coerce("0").unwrap(),
            serde_json::json!(false)
        );
        assert!(var(VarType::Bool, None).coerce("maybe").is_err());

        let choices = Some(vec!["a".to_string(), "b".to_string()]);
        assert!(var(VarType::String, choices.clone()).coerce("a").is_ok());
        assert!(var(VarType::String, choices).coerce("c").is_err());
    }

    fn assert_std_error<E: std::error::Error>() {}

    #[test]
    fn var_coerce_error_is_a_std_error() {
        assert_std_error::<VarCoerceError>();
    }
}
