//! Built-in agent for [pi](https://github.com/badlogic/pi-mono).

use crate::config::AgentConfig;

use super::agent::{Agent, AgentContext, AgentDescriptor, CommandSpec, Invocation};
use super::configurable::{capabilities_from_config, overlay, TemplateAgent};

/// The built-in defaults, matching `config.example.toml`.
fn base_config() -> AgentConfig {
    AgentConfig {
        command: "pi".to_string(),
        args: Vec::new(),
        // pi submits a positional initial message itself, so `submit_prompt`
        // stays unset (same as `claude`).
        prompt_args: Some(vec!["{prompt}".to_string()]),
        // A deterministic `{session_id}` is bound to every headless run so the
        // created session can later be reopened with `resume_args`.
        headless_args: Some(vec![
            "-p".to_string(),
            "--session-id".to_string(),
            "{session_id}".to_string(),
            "{prompt}".to_string(),
        ]),
        run_args: Some(vec![
            "-p".to_string(),
            "--provider".to_string(),
            "{provider}".to_string(),
            "--model".to_string(),
            "{model}".to_string(),
            "--session-id".to_string(),
            "{session_id}".to_string(),
            "{prompt}".to_string(),
        ]),
        resume_args: Some(vec!["--session".to_string(), "{session_id}".to_string()]),
        interactive_model_args: Some(vec![
            "--provider".to_string(),
            "{provider}".to_string(),
            "--model".to_string(),
            "{model}".to_string(),
        ]),
        ..Default::default()
    }
}

/// The pi implementation: interactive and headless (with or without a model).
pub struct PiAgent {
    template: TemplateAgent,
}

impl PiAgent {
    /// Build pi for the `[agents.<name>]` entry `overrides`.
    pub fn from_config(name: &str, overrides: &AgentConfig) -> Self {
        let mut config = base_config();
        overlay(&mut config, overrides);
        let capabilities = capabilities_from_config(&config);
        let descriptor = AgentDescriptor {
            id: name.to_string(),
            name: "Pi".to_string(),
            command: config.command.clone(),
            available: true,
            capabilities,
        };
        Self {
            template: TemplateAgent {
                descriptor,
                config,
                probe: None,
            },
        }
    }
}

impl Agent for PiAgent {
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

    fn set_available(&mut self, available: bool) {
        self.template.descriptor.available = available;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::agent::SubmitStrategy;

    fn ctx(prompt: Option<&str>, provider: Option<&str>, model: Option<&str>) -> AgentContext {
        AgentContext {
            prompt: prompt.map(str::to_string),
            provider: provider.map(str::to_string),
            model: model.map(str::to_string),
            // A headless run always carries a deterministic id.
            session_id: Some("ses-1234".to_string()),
            ..Default::default()
        }
    }

    fn agent() -> PiAgent {
        PiAgent::from_config("pi", &AgentConfig::default())
    }

    #[test]
    fn capabilities_include_headless_and_model() {
        let caps = agent().capabilities();
        assert!(caps.interactive);
        assert!(caps.headless);
        assert!(caps.model_selection);
        assert!(caps.resume);
        assert!(!caps.providers);
        assert!(!caps.structured_output);
        assert!(!caps.reports_session_id);
        assert!(!caps.prompt_prefill);
    }

    #[test]
    fn interactive_launches_bare_command() {
        let spec = agent()
            .command(
                &Invocation::Interactive {
                    prompt: None,
                    provider: None,
                    model: None,
                },
                &ctx(None, None, None),
            )
            .unwrap();
        assert_eq!(spec.program, std::path::PathBuf::from("pi"));
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
                &ctx(Some("hi"), None, None),
            )
            .unwrap();
        assert_eq!(spec.args, vec!["hi"]);
        assert!(spec.stdin_prompt.is_none());
        assert!(matches!(spec.submit, SubmitStrategy::None));
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
    fn headless_without_model_uses_headless_args() {
        let spec = agent()
            .command(
                &Invocation::Headless {
                    prompt: "do it",
                    provider: None,
                    model: None,
                },
                &ctx(Some("do it"), None, None),
            )
            .unwrap();
        assert_eq!(spec.args, vec!["-p", "--session-id", "ses-1234", "do it"]);
        assert!(spec.stdin_eof);
        assert!(spec.stdin_prompt.is_none());
    }

    #[test]
    fn headless_with_model_uses_run_args() {
        let spec = agent()
            .command(
                &Invocation::Headless {
                    prompt: "do it",
                    provider: Some("mistral"),
                    model: Some("large"),
                },
                &ctx(Some("do it"), Some("mistral"), Some("large")),
            )
            .unwrap();
        assert_eq!(
            spec.args,
            vec![
                "-p",
                "--provider",
                "mistral",
                "--model",
                "large",
                "--session-id",
                "ses-1234",
                "do it"
            ]
        );
    }

    #[test]
    fn resume_reopens_a_session_interactively() {
        let spec = agent()
            .command(&Invocation::Resume("ses-1234"), &ctx(None, None, None))
            .unwrap();
        assert_eq!(spec.args, vec!["--session", "ses-1234"]);
        assert!(spec.stdin_prompt.is_none());
    }
}
