//! The `supervise_workflow` prompt: the one-decision-per-cycle contract an MCP
//! client should follow when supervising a favetto workflow.

use serde_json::{json, Value};

use crate::tools::ToolError;

/// Name of the single prompt this server exposes.
pub const SUPERVISE_WORKFLOW: &str = "supervise_workflow";

/// A prompt declaration for `prompts/list`.
#[derive(Debug, Clone, PartialEq)]
pub struct PromptSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub arguments: Vec<PromptArgumentSpec>,
}

/// One prompt argument.
#[derive(Debug, Clone, PartialEq)]
pub struct PromptArgumentSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub required: bool,
}

/// The single prompt this server exposes.
pub fn list() -> Vec<PromptSpec> {
    vec![PromptSpec {
        name: SUPERVISE_WORKFLOW,
        description: "Drive one favetto workflow root to a terminal decision using the closed \
                      supervisor vocabulary.",
        arguments: vec![PromptArgumentSpec {
            name: "root_id",
            description: "The workflow root task id to supervise (optional; if absent, start one with favetto_start_task).",
            required: false,
        }],
    }]
}

/// Build the `prompts/get` result for `name`.
pub fn get(name: &str, args: &Value) -> Result<Value, ToolError> {
    if name != SUPERVISE_WORKFLOW {
        return Err(ToolError::new(format!("unknown prompt `{name}`")));
    }
    let root_id = args.get("root_id").and_then(Value::as_str);
    let mut text = String::from(CONTRACT);
    if let Some(root_id) = root_id {
        text.push_str("\n\nSupervise this workflow root: ");
        text.push_str(root_id);
        text.push('.');
    }
    Ok(json!({
        "description": "One decision per cycle, then re-inspect.",
        "messages": [{
            "role": "user",
            "content": { "type": "text", "text": text },
        }],
    }))
}

/// The static contract text handed to the model.
const CONTRACT: &str = "You are supervising a favetto workflow. favetto is a deterministic \
executor: it validates and runs work, it never decides. All judgment is yours.\n\n\
Observe, then emit EXACTLY ONE decision per cycle, apply it, then re-inspect before deciding \
again. The decision vocabulary is closed: inspect, spawn, cancel, retry, wait, request_input, \
complete, escalate. Do not invent actions.\n\n\
Map each action to exactly one tool call:\n\
- inspect -> favetto_inspect { root_id }\n\
- spawn -> favetto_spawn (or favetto_create_workflow for a whole DAG, favetto_start_task for a new root)\n\
- cancel -> favetto_cancel_workflow { root_id } (or favetto_cancel_task for one task)\n\
- retry -> favetto_retry_task { task_id }\n\
- wait -> favetto_wait { until?, timeout_ms? }\n\
- request_input -> surface the pending question to the operator with favetto_agents_list and \
answer it with favetto_agents_reply; then wait\n\
- complete -> stop supervising; the daemon is not told\n\
- escalate -> stop autonomous control and hand off to a human\n\n\
Rules:\n\
- Never write raw PTY input: agents.input is not available. The only agent input tool is \
favetto_agents_reply, and only for answering a structured input request.\n\
- wait until in-flight work finishes before spawning more: inspect `state` and the per-task \
`status` first.\n\
- Re-inspect between mutations; do not emit two mutations in one cycle.\n\
- Use only catalog task names that exist; prefer a `dedupe_key` for spawns so a retried decision \
cannot double-start work.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_listing_declares_root_id() {
        let prompts = list();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].name, SUPERVISE_WORKFLOW);
        assert_eq!(prompts[0].arguments[0].name, "root_id");
        assert!(!prompts[0].arguments[0].required);
    }

    #[test]
    fn prompt_text_names_every_action_and_the_one_decision_rule() {
        let result = get(SUPERVISE_WORKFLOW, &json!({})).unwrap();
        let text = result["messages"][0]["content"]["text"].as_str().unwrap();
        for action in [
            "inspect",
            "spawn",
            "cancel",
            "retry",
            "wait",
            "request_input",
            "complete",
            "escalate",
        ] {
            assert!(text.contains(action), "prompt is missing `{action}`");
        }
        assert!(text.contains("EXACTLY ONE decision per cycle"), "{text}");
        assert!(text.contains("Never write raw PTY input"), "{text}");
        assert!(text.contains("favetto_agents_reply"), "{text}");
    }

    #[test]
    fn prompt_includes_the_root_when_given() {
        let result = get(SUPERVISE_WORKFLOW, &json!({"root_id": "abc"})).unwrap();
        let text = result["messages"][0]["content"]["text"].as_str().unwrap();
        assert!(text.contains("Supervise this workflow root: abc."));
    }

    #[test]
    fn unknown_prompt_errors() {
        assert!(get("nope", &json!({})).is_err());
    }
}
