//! Evaluation of the structured `[action.when]` predicate.
//!
//! The predicate is deliberately small and non-Turing-complete: resolve a path
//! into the action's `outputs` and apply one comparison. It only reads the
//! result, so a `kind = "action"` poller stays deterministic and reproducible.

use std::cmp::Ordering;

use serde_json::Value;

use crate::tasks::{toml_to_json, WhenOp, WhenPredicate};

/// Whether `when` holds for the given action `outputs`.
///
/// A missing path resolves to JSON `null`, so `empty`/`ne` are true and
/// `not_empty`/`eq` are false for an absent key.
pub fn evaluate_when(when: &WhenPredicate, outputs: &Value) -> anyhow::Result<bool> {
    let resolved = resolve_path(outputs, &when.path)
        .cloned()
        .unwrap_or(Value::Null);
    let operand = when.value.as_ref().map(toml_to_json).unwrap_or(Value::Null);
    let result = match when.op {
        WhenOp::NotEmpty => !is_empty_value(&resolved),
        WhenOp::Empty => is_empty_value(&resolved),
        WhenOp::Eq => resolved == operand,
        WhenOp::Ne => resolved != operand,
        WhenOp::Gt => compare_ordering(&resolved, &operand) == Some(Ordering::Greater),
        WhenOp::Lt => compare_ordering(&resolved, &operand) == Some(Ordering::Less),
        WhenOp::Contains => contains(&resolved, &operand),
    };
    Ok(result)
}

/// Resolve a dot path or a `/`-prefixed JSON Pointer against `root`. An empty
/// path resolves to the root itself.
fn resolve_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let path = path.trim();
    if path.is_empty() {
        return Some(root);
    }
    if path.starts_with('/') {
        return root.pointer(path);
    }
    let mut current = root;
    for segment in path.split('.') {
        if segment.is_empty() {
            continue;
        }
        current = match current {
            Value::Object(map) => map.get(segment)?,
            Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

/// Whether a resolved value counts as "empty": `null`, an empty string, array,
/// or object. Scalars (numbers, booleans) are always considered present.
fn is_empty_value(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
        Value::Bool(_) | Value::Number(_) => false,
    }
}

/// Ordering for `gt`/`lt`: numbers compare numerically, strings lexically.
/// Incomparable types yield `None`, so the comparison is false.
fn compare_ordering(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64().partial_cmp(&y.as_f64()),
        (Value::String(x), Value::String(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

/// `contains` for `eq`-style operands: a substring of a string, an element of an
/// array, or a key of an object.
fn contains(haystack: &Value, needle: &Value) -> bool {
    match haystack {
        Value::String(s) => needle.as_str().is_some_and(|n| s.contains(n)),
        Value::Array(items) => items.contains(needle),
        Value::Object(map) => needle.as_str().is_some_and(|k| map.contains_key(k)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::WhenPredicate;

    fn predicate(path: &str, op: WhenOp, value: Option<Value>) -> WhenPredicate {
        WhenPredicate {
            path: path.to_string(),
            op,
            value: value.map(json_to_toml),
        }
    }

    /// A tiny JSON -> TOML literal converter for test predicates.
    fn json_to_toml(value: Value) -> toml::Value {
        match value {
            Value::Null => toml::Value::String(String::new()),
            Value::Bool(b) => toml::Value::Boolean(b),
            Value::Number(n) if n.is_i64() => toml::Value::Integer(n.as_i64().unwrap()),
            Value::Number(n) => toml::Value::Float(n.as_f64().unwrap()),
            Value::String(s) => toml::Value::String(s),
            Value::Array(items) => {
                toml::Value::Array(items.into_iter().map(json_to_toml).collect())
            }
            Value::Object(map) => toml::Value::Table(
                map.into_iter()
                    .map(|(key, value)| (key, json_to_toml(value)))
                    .collect(),
            ),
        }
    }

    fn outputs() -> Value {
        serde_json::json!({
            "status": 200,
            "items": [{"number": 1}, {"number": 2}],
            "empty_list": [],
            "label": "hello world",
            "nested": { "deep": { "value": 3 } },
        })
    }

    #[test]
    fn not_empty_and_empty_handle_absent_and_empty_values() {
        let out = outputs();
        assert!(evaluate_when(&predicate("items", WhenOp::NotEmpty, None), &out).unwrap());
        assert!(!evaluate_when(&predicate("empty_list", WhenOp::NotEmpty, None), &out).unwrap());
        assert!(!evaluate_when(&predicate("missing", WhenOp::NotEmpty, None), &out).unwrap());
        assert!(evaluate_when(&predicate("missing", WhenOp::Empty, None), &out).unwrap());
        assert!(evaluate_when(&predicate("empty_list", WhenOp::Empty, None), &out).unwrap());
    }

    #[test]
    fn eq_and_ne_compare_scalars() {
        let out = outputs();
        assert!(evaluate_when(
            &predicate("status", WhenOp::Eq, Some(Value::from(200))),
            &out
        )
        .unwrap());
        assert!(evaluate_when(
            &predicate("status", WhenOp::Ne, Some(Value::from(404))),
            &out
        )
        .unwrap());
        assert!(!evaluate_when(
            &predicate("status", WhenOp::Eq, Some(Value::from(404))),
            &out
        )
        .unwrap());
    }

    #[test]
    fn gt_and_lt_compare_numbers_and_strings() {
        let out = outputs();
        assert!(evaluate_when(
            &predicate("status", WhenOp::Gt, Some(Value::from(100))),
            &out
        )
        .unwrap());
        assert!(evaluate_when(
            &predicate("status", WhenOp::Lt, Some(Value::from(300))),
            &out
        )
        .unwrap());
        assert!(evaluate_when(
            &predicate("label", WhenOp::Lt, Some(Value::from("z"))),
            &out
        )
        .unwrap());
        // Incomparable types are false, not an error.
        assert!(!evaluate_when(
            &predicate("status", WhenOp::Gt, Some(Value::from("x"))),
            &out
        )
        .unwrap());
    }

    #[test]
    fn contains_handles_strings_arrays_and_objects() {
        let out = outputs();
        assert!(evaluate_when(
            &predicate("label", WhenOp::Contains, Some(Value::from("world"))),
            &out
        )
        .unwrap());
        assert!(evaluate_when(
            &predicate(
                "items",
                WhenOp::Contains,
                Some(serde_json::json!({"number": 2}))
            ),
            &out
        )
        .unwrap());
        assert!(evaluate_when(
            &predicate("nested", WhenOp::Contains, Some(Value::from("deep"))),
            &out
        )
        .unwrap());
    }

    #[test]
    fn resolves_dot_paths_json_pointers_and_array_indexes() {
        let out = outputs();
        assert_eq!(
            resolve_path(&out, "nested.deep.value"),
            Some(&Value::from(3))
        );
        assert_eq!(
            resolve_path(&out, "/nested/deep/value"),
            Some(&Value::from(3))
        );
        assert_eq!(resolve_path(&out, "items.1.number"), Some(&Value::from(2)));
        assert_eq!(resolve_path(&out, ""), Some(&out));
        assert_eq!(resolve_path(&out, "nested.missing"), None);
    }
}
