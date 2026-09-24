//! Minimal `{{ dotted.path }}` template rendering for task prompts and handoff
//! paths.
//!
//! Placeholders are resolved against a `serde_json::Value` context:
//!
//! - a JSON string renders as its raw contents,
//! - numbers/bools/null render as their JSON literal (null as empty),
//! - objects/arrays render as compact JSON,
//! - a missing path renders as the empty string.
//!
//! Double braces are required so braces inside prompt code blocks (JSON, Rust,
//! shell) are left untouched.

use serde_json::Value;

/// Render `{{ path.to.value }}` placeholders in `template` against `context`.
pub fn render(template: &str, context: &Value) -> String {
    let mut out = String::with_capacity(template.len());
    let mut i = 0;
    while i < template.len() {
        if template[i..].starts_with("{{") {
            if let Some(end) = template[i + 2..].find("}}") {
                let path = template[i + 2..i + 2 + end].trim();
                out.push_str(&resolve(context, path));
                i = i + 2 + end + 2;
                continue;
            }
        }
        let ch = template[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Resolve a dotted path against the context, returning its rendered string.
fn resolve(context: &Value, path: &str) -> String {
    let mut current = context;
    for part in path.split('.').filter(|p| !p.is_empty()) {
        match current.get(part) {
            Some(v) => current = v,
            None => return String::new(),
        }
    }
    match current {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_dotted_paths() {
        let ctx = json!({
            "task": { "id": "abc" },
            "input": { "issue_id": 42, "title": "fix it", "flag": true },
            "prev": { "output": "done" },
        });
        assert_eq!(
            render("plan {{ input.issue_id }}: {{ input.title }}", &ctx),
            "plan 42: fix it"
        );
        assert_eq!(render("{{ task.id }}", &ctx), "abc");
        assert_eq!(render("{{ prev.output }}", &ctx), "done");
        assert_eq!(render("{{ input.flag }}", &ctx), "true");
    }

    #[test]
    fn missing_path_is_empty_and_braces_survive() {
        let ctx = json!({ "input": {} });
        assert_eq!(render("x{{ nope.deep }}y", &ctx), "xy");
        assert_eq!(render("{\"a\": 1}", &ctx), "{\"a\": 1}");
    }

    #[test]
    fn objects_render_as_json() {
        let ctx = json!({ "input": { "labels": ["a", "b"] } });
        assert_eq!(render("{{ input.labels }}", &ctx), "[\"a\",\"b\"]");
    }
}
