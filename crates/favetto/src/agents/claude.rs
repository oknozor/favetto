//! Built-in agent for Claude Code.

use favetto_core::model::AwaitingInputReason;

use crate::agent_hooks::{AgentHookLaunch, HookInjection};
use crate::config::AgentConfig;

use super::agent::{Agent, AgentRunResult};
use super::configurable::{delegate_to_template, overlay, TemplateAgent};
use super::state::{StateSource, StateSourceConfig};

mod hooks;
mod stream_json;
use stream_json::ClaudeStreamJsonParser;

/// The built-in defaults, matching `config.example.toml`.
fn base_config() -> AgentConfig {
    AgentConfig {
        command: "claude".to_string(),
        args: Vec::new(),
        prompt_args: Some(vec!["{prompt}".to_string()]),
        headless_args: Some(vec![
            "-p".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
            "--include-partial-messages".to_string(),
            "{prompt}".to_string(),
        ]),
        run_args: Some(vec![
            "-p".to_string(),
            "--model".to_string(),
            "{model}".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
            "--include-partial-messages".to_string(),
            "{prompt}".to_string(),
        ]),
        interactive_model_args: Some(vec!["--model".to_string(), "{model}".to_string()]),
        session_id_json_key: Some("session_id".to_string()),
        ..Default::default()
    }
}

/// The Claude Code implementation.
pub struct ClaudeAgent {
    template: TemplateAgent,
}

impl ClaudeAgent {
    /// Build Claude for the `[agents.<name>]` entry `overrides`.
    pub fn from_config(name: &str, overrides: &AgentConfig) -> Self {
        let mut config = base_config();
        overlay(&mut config, overrides);
        let mut template = TemplateAgent::new(name, "Claude", config);
        // Claude reports live state (stream-json and hooks), but interactive
        // sessions are observe-only: the hook receiver never answers a prompt.
        template.descriptor.capabilities.reports_state = true;
        template.descriptor.capabilities.permission_channel = false;
        Self { template }
    }
}

impl Agent for ClaudeAgent {
    delegate_to_template!();

    fn parse_output(&self, raw: &str, exit_code: Option<i32>) -> AgentRunResult {
        let mut parser = ClaudeStreamJsonParser::new();
        parser.push(raw.as_bytes());
        parser.finish(exit_code);
        let summary = parser.summary();
        if summary.outcome.is_none() && summary.text.is_empty() && summary.tool_calls.is_empty() {
            // No structured rows: keep the interactive screen text as-is.
            return AgentRunResult {
                exit_code,
                session_id: summary.session_id.clone(),
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
        hooks::state_source(cfg)
    }

    fn hook_injection(&self, launch: &AgentHookLaunch) -> Option<HookInjection> {
        hooks::injection(launch)
    }

    fn awaiting_input(&self, screen: &vt100::Screen) -> Option<AwaitingInputReason> {
        super::detect::claude_awaiting_input(&screen.contents())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::agent::{AgentContext, Invocation};

    fn ctx(prompt: Option<&str>, model: Option<&str>) -> AgentContext {
        AgentContext {
            prompt: prompt.map(str::to_string),
            model: model.map(str::to_string),
            ..Default::default()
        }
    }

    fn agent() -> ClaudeAgent {
        ClaudeAgent::from_config("claude", &AgentConfig::default())
    }

    #[test]
    fn capabilities_match_the_example_config() {
        let caps = agent().capabilities();
        assert!(caps.interactive);
        assert!(caps.headless);
        assert!(caps.model_selection);
        assert!(!caps.resume);
        assert!(!caps.providers);
        assert!(caps.reports_session_id);
        assert!(caps.structured_output);
        assert!(caps.reports_state);
        assert!(!caps.permission_channel);
    }

    #[test]
    fn empty_base_config_for_interactive_without_prompt() {
        let spec = agent()
            .command(
                &Invocation::Interactive {
                    prompt: None,
                    provider: None,
                    model: None,
                },
                &ctx(None, None),
            )
            .unwrap();
        assert_eq!(spec.program, std::path::PathBuf::from("claude"));
        assert!(spec.args.is_empty());
    }

    #[test]
    fn interactive_prompt_uses_prompt_args() {
        let spec = agent()
            .command(
                &Invocation::Interactive {
                    prompt: Some("hi"),
                    provider: None,
                    model: None,
                },
                &ctx(Some("hi"), None),
            )
            .unwrap();
        assert_eq!(spec.args, vec!["hi"]);
        assert!(spec.stdin_prompt.is_none());
    }

    #[test]
    fn interactive_model_uses_interactive_model_args() {
        let spec = agent()
            .command(
                &Invocation::Interactive {
                    prompt: None,
                    provider: None,
                    model: Some("sonnet"),
                },
                &ctx(None, Some("sonnet")),
            )
            .unwrap();
        assert_eq!(spec.args, vec!["--model", "sonnet"]);
    }

    #[test]
    fn headless_paths_match_the_example_config() {
        let stream = [
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
        ];
        let no_model = agent()
            .command(
                &Invocation::Headless {
                    prompt: "do it",
                    provider: None,
                    model: None,
                },
                &ctx(Some("do it"), None),
            )
            .unwrap();
        let mut expected = vec!["-p"];
        expected.extend(stream);
        expected.push("do it");
        assert_eq!(no_model.args, expected);

        let with_model = agent()
            .command(
                &Invocation::Headless {
                    prompt: "do it",
                    provider: None,
                    model: Some("sonnet"),
                },
                &ctx(Some("do it"), Some("sonnet")),
            )
            .unwrap();
        let mut expected = vec!["-p", "--model", "sonnet"];
        expected.extend(stream);
        expected.push("do it");
        assert_eq!(with_model.args, expected);
    }

    #[test]
    fn parse_output_returns_a_run_summary() {
        let raw = concat!(
            r#"{"type":"system","subtype":"init","session_id":"ses_9","model":"claude"}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"done","session_id":"ses_9","total_cost_usd":0.01,"usage":{"input_tokens":2}}"#,
            "\n",
        );
        let result = agent().parse_output(raw, Some(0));
        assert_eq!(result.session_id.as_deref(), Some("ses_9"));
        assert_eq!(result.output["session_id"], "ses_9");
        assert_eq!(result.output["text"], "done");
        assert_eq!(result.output["outcome"], "succeeded");
        assert_eq!(result.output["usage"]["input_tokens"], 2);
        assert_eq!(result.raw, raw);
    }

    #[test]
    fn parse_output_falls_back_to_text_for_screen_output() {
        let result = agent().parse_output("hello from the TUI\n", Some(0));
        assert_eq!(
            result.output,
            serde_json::json!({ "text": "hello from the TUI\n" })
        );
        assert!(result.session_id.is_none());
        assert_eq!(result.raw, "hello from the TUI\n");
    }

    #[test]
    fn hook_injection_writes_a_settings_file() {
        let dir = std::env::temp_dir().join(format!("favetto-claude-inj-{}", uuid::Uuid::new_v4()));
        let launch = AgentHookLaunch {
            endpoint: "http://127.0.0.1:7878/agent-hooks/claude".to_string(),
            dir: dir.clone(),
        };
        let injection = agent().hook_injection(&launch).expect("injection");
        assert_eq!(injection.args[0], "--settings");
        assert!(std::path::Path::new(&injection.args[1]).exists());
        assert!(injection.env.contains_key(hooks::HOOK_URL_ENV));
        assert!(injection.env.contains_key(hooks::HOOK_SETTINGS_ENV));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_confirmation_dialog_once_screen_rendered() {
        use favetto_core::model::AwaitingInputKind;
        let mut parser = vt100::Parser::new(12, 70, 0);
        parser
            .process("Do you want to proceed?\r\n❯ 1. Yes\r\n  2. No\r\nEsc to cancel".as_bytes());
        let reason = agent().awaiting_input(parser.screen());
        assert_eq!(
            reason.map(|r| r.kind),
            Some(AwaitingInputKind::Confirmation)
        );
    }

    #[test]
    fn idle_screen_is_not_awaiting() {
        let mut parser = vt100::Parser::new(12, 70, 0);
        parser.process(b"Ready for your next task");
        assert!(agent().awaiting_input(parser.screen()).is_none());
    }
}
