//! Built-in agent for Claude Code.

use crate::config::AgentConfig;

use super::agent::{Agent, AgentContext, AgentDescriptor, CommandSpec, Invocation, SessionIdProbe};
use super::configurable::{capabilities_from_config, overlay, TemplateAgent};

/// The built-in defaults, matching `config.example.toml`.
fn base_config() -> AgentConfig {
    AgentConfig {
        command: "claude".to_string(),
        args: Vec::new(),
        prompt_args: Some(vec!["{prompt}".to_string()]),
        headless_args: Some(vec!["-p".to_string(), "{prompt}".to_string()]),
        run_args: Some(vec![
            "-p".to_string(),
            "--model".to_string(),
            "{model}".to_string(),
            "{prompt}".to_string(),
        ]),
        interactive_model_args: Some(vec!["--model".to_string(), "{model}".to_string()]),
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
        let capabilities = capabilities_from_config(&config);
        let probe = config
            .session_id_json_key
            .clone()
            .map(SessionIdProbe::JsonKey);
        let descriptor = AgentDescriptor {
            id: name.to_string(),
            name: "Claude".to_string(),
            command: config.command.clone(),
            available: true,
            capabilities,
        };
        Self {
            template: TemplateAgent {
                descriptor,
                config,
                probe,
            },
        }
    }
}

impl Agent for ClaudeAgent {
    fn descriptor(&self) -> &AgentDescriptor {
        &self.template.descriptor
    }

    fn command(
        &self,
        invocation: &Invocation<'_>,
        ctx: &AgentContext,
    ) -> anyhow::Result<CommandSpec> {
        self.template.command(invocation, ctx)
    }

    fn session_id_probe(&self) -> Option<SessionIdProbe> {
        self.template.probe.clone()
    }

    fn set_available(&mut self, available: bool) {
        self.template.descriptor.available = available;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!caps.reports_session_id);
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
        assert_eq!(no_model.args, vec!["-p", "do it"]);

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
        assert_eq!(with_model.args, vec!["-p", "--model", "sonnet", "do it"]);
    }
}
