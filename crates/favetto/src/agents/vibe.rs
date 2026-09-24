//! Built-in agent for Mistral Vibe.
//!
//! Headless runs launch Vibe with `--output streaming` and attach a
//! [`VibeStreamSource`](stream::VibeStreamSource), so a task reports its
//! session id, assistant text, reasoning, tool calls, and usage through the
//! structured `StateSource` seam. Interactive Vibe keeps the bare TUI and the
//! debounced screen fallback; it does not attach a state source. Vibe has no
//! structured permission channel.

use std::path::Path;

use crate::config::AgentConfig;

use super::agent::{Agent, AgentRunResult};
use super::configurable::{delegate_to_template, overlay, TemplateAgent};
use super::state::{StateSource, StateSourceConfig};

mod session_index;
mod stream;
use stream::state_source;

/// The built-in defaults, matching `config.example.toml`.
fn base_config() -> AgentConfig {
    AgentConfig {
        command: "vibe".to_string(),
        args: Vec::new(),
        prompt_args: Some(vec!["{prompt}".to_string()]),
        submit_prompt: Some(true),
        headless_args: Some(vec![
            "-p".to_string(),
            "{prompt}".to_string(),
            "--output".to_string(),
            "streaming".to_string(),
            "--auto-approve".to_string(),
        ]),
        run_args: Some(vec![
            "-p".to_string(),
            "--provider".to_string(),
            "{provider}".to_string(),
            "--model".to_string(),
            "{model}".to_string(),
            "{prompt}".to_string(),
            "--output".to_string(),
            "streaming".to_string(),
            "--auto-approve".to_string(),
        ]),
        interactive_model_args: Some(vec![
            "--provider".to_string(),
            "{provider}".to_string(),
            "--model".to_string(),
            "{model}".to_string(),
        ]),
        ..Default::default()
    }
}

/// The Mistral Vibe implementation.
pub struct VibeAgent {
    template: TemplateAgent,
}

impl VibeAgent {
    /// Build Vibe for the `[agents.<name>]` entry `overrides`.
    pub fn from_config(name: &str, overrides: &AgentConfig) -> Self {
        let mut config = base_config();
        overlay(&mut config, overrides);
        let mut template = TemplateAgent::new(name, "Vibe", config);
        // Headless runs report live state over `--output streaming`; interactive
        // sessions keep the screen heuristic, and Vibe has no permission
        // channel (`permission_channel` stays at its default `false`).
        template.descriptor.capabilities.reports_state = true;
        Self { template }
    }
}

impl Agent for VibeAgent {
    delegate_to_template!();

    fn parse_output(&self, raw: &str, exit_code: Option<i32>) -> AgentRunResult {
        let mut parser = stream::VibeStreamParser::new();
        parser.push(raw.as_bytes());
        parser.finish(exit_code);
        let summary = parser.summary();
        if summary.outcome.is_none() && summary.text.is_empty() && summary.tool_calls.is_empty() {
            // Unstructured output (e.g. an overridden headless invocation):
            // keep the raw text exactly as the default used to.
            return AgentRunResult {
                exit_code,
                session_id: None,
                output: serde_json::json!({ "text": raw }),
                raw: raw.to_string(),
            };
        }
        AgentRunResult {
            exit_code,
            session_id: summary.session_id.clone(),
            output: serde_json::to_value(summary).unwrap_or(serde_json::Value::Null),
            raw: raw.to_string(),
        }
    }

    fn state_source(&self, cfg: &StateSourceConfig) -> Option<Box<dyn StateSource>> {
        state_source(cfg)
    }

    fn has_session_titles(&self) -> bool {
        true
    }

    fn session_title(&self, session_id: &str, _cwd: &Path) -> Option<String> {
        session_index::find_session(session_id).and_then(|session| session.title)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::agent::{AgentContext, Invocation, SubmitStrategy};
    use crate::agents::state::StateSourceConfig;

    fn ctx(prompt: Option<&str>, provider: Option<&str>, model: Option<&str>) -> AgentContext {
        AgentContext {
            prompt: prompt.map(str::to_string),
            provider: provider.map(str::to_string),
            model: model.map(str::to_string),
            ..Default::default()
        }
    }

    fn agent() -> VibeAgent {
        VibeAgent::from_config("vibe", &AgentConfig::default())
    }

    fn state_config(headless: bool, args: Vec<&str>) -> StateSourceConfig {
        StateSourceConfig {
            headless,
            args: args.into_iter().map(str::to_string).collect(),
            ..Default::default()
        }
    }

    fn headless_spec(
        provider: Option<&str>,
        model: Option<&str>,
    ) -> crate::agents::agent::CommandSpec {
        agent()
            .command(
                &Invocation::Headless {
                    prompt: "do it",
                    provider,
                    model,
                },
                &ctx(Some("do it"), provider, model),
            )
            .unwrap()
    }

    #[test]
    fn capabilities_match_the_example_config() {
        let caps = agent().capabilities();
        assert!(caps.interactive);
        assert!(caps.headless);
        assert!(caps.model_selection);
        assert!(caps.prompt_prefill);
        assert!(!caps.providers);
        assert!(!caps.resume);
        assert!(!caps.reports_session_id);
        assert!(caps.reports_state);
        assert!(!caps.permission_channel);
    }

    #[test]
    fn interactive_prompt_is_prefilled_and_submitted() {
        let spec = agent()
            .command(
                &Invocation::Interactive {
                    prompt: Some("hi"),
                    provider: None,
                    model: None,
                },
                &ctx(Some("hi"), None, None),
            )
            .unwrap();
        assert_eq!(spec.args, vec!["hi"]);
        assert!(matches!(spec.submit, SubmitStrategy::AfterSettle { .. }));
    }

    #[test]
    fn interactive_model_uses_provider_and_model() {
        let spec = agent()
            .command(
                &Invocation::Interactive {
                    prompt: None,
                    provider: Some("mistral"),
                    model: Some("large"),
                },
                &ctx(None, Some("mistral"), Some("large")),
            )
            .unwrap();
        assert_eq!(spec.args, vec!["--provider", "mistral", "--model", "large"]);
    }

    #[test]
    fn headless_paths_include_streaming_output() {
        let no_model = headless_spec(None, None);
        assert_eq!(
            no_model.args,
            vec!["-p", "do it", "--output", "streaming", "--auto-approve"]
        );

        let with_model = headless_spec(Some("mistral"), Some("large"));
        assert_eq!(
            with_model.args,
            vec![
                "-p",
                "--provider",
                "mistral",
                "--model",
                "large",
                "do it",
                "--output",
                "streaming",
                "--auto-approve"
            ]
        );
    }

    #[test]
    fn headless_uses_a_state_source() {
        let spec = headless_spec(None, None);
        let source = agent().state_source(&state_config(
            true,
            spec.args.iter().map(|s| s.as_str()).collect(),
        ));
        assert!(source.is_some());
        assert_eq!(source.unwrap().label(), "vibe-streaming");
    }

    #[test]
    fn interactive_has_no_state_source() {
        // The interactive TUI keeps the screen heuristic.
        assert!(agent().state_source(&state_config(false, vec![])).is_none());
        // A headless launch that is not streaming (a user override) does too.
        assert!(agent()
            .state_source(&state_config(
                true,
                vec!["-p", "{prompt}", "--auto-approve"]
            ))
            .is_none());
    }

    #[test]
    fn parse_output_summarizes_a_streaming_run() {
        let raw = concat!(
            r#"{"id":"assistant-1","sessionId":"ses_vibe","turnId":"turn-1","type":"message","role":"assistant","content":[{"type":"text","text":"done"}]}"#,
            "\n",
            r#"{"type":"result","is_error":false,"result":"done","total_cost_usd":0.02,"usage":{"input_tokens":7,"output_tokens":3}}"#,
            "\n",
        );
        let result = agent().parse_output(raw, Some(0));
        assert_eq!(result.session_id.as_deref(), Some("ses_vibe"));
        assert_eq!(result.output["text"], "done");
        assert_eq!(result.output["outcome"], "succeeded");
        assert_eq!(result.output["usage"]["input_tokens"], 7);
        assert_eq!(result.output["usage"]["output_tokens"], 3);
        assert_eq!(result.output["usage"]["cost_usd"], 0.02);
        assert_eq!(result.raw, raw);
    }

    #[test]
    fn parse_output_falls_back_to_raw_text() {
        let result = agent().parse_output("plain vibe output\n", Some(0));
        assert_eq!(
            result.output,
            serde_json::json!({ "text": "plain vibe output\n" })
        );
        assert!(result.session_id.is_none());
        assert_eq!(result.raw, "plain vibe output\n");
    }

    #[test]
    fn session_title_reads_the_index() {
        let root = std::env::temp_dir().join(format!(
            "favetto-vibe-title-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let sessions = root.join("logs").join("session");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join(".session_index.json"),
            serde_json::to_vec(&serde_json::json!({
                "session_20260924_100000_abcd": {
                    "session_id": "ses_titled",
                    "cwd": "/home/me/proj",
                    "start_time": "2026-09-24T10:00:00+00:00",
                    "mtime_ns": 1_i64,
                    "title": "Fix the bug"
                }
            }))
            .unwrap(),
        )
        .unwrap();

        // `VIBE_HOME` is the only env reader in the vibe tests; keep it to this
        // one self-contained case.
        std::env::set_var("VIBE_HOME", &root);
        let title = agent().session_title("ses_titled", Path::new("/home/me/proj"));
        std::env::remove_var("VIBE_HOME");

        assert_eq!(title.as_deref(), Some("Fix the bug"));
        assert!(agent().has_session_titles());
        let _ = std::fs::remove_dir_all(&root);
    }
}
