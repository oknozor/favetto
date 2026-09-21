//! Built-in agent for Mistral Vibe.

use crate::config::AgentConfig;

use super::agent::{Agent, AgentContext, AgentDescriptor, CommandSpec, Invocation, SessionIdProbe};
use super::configurable::{capabilities_from_config, overlay, TemplateAgent};

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
            "--auto-approve".to_string(),
        ]),
        run_args: Some(vec![
            "-p".to_string(),
            "--provider".to_string(),
            "{provider}".to_string(),
            "--model".to_string(),
            "{model}".to_string(),
            "{prompt}".to_string(),
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
        let capabilities = capabilities_from_config(&config);
        let probe = config
            .session_id_json_key
            .clone()
            .map(SessionIdProbe::JsonKey);
        let descriptor = AgentDescriptor {
            id: name.to_string(),
            name: "Vibe".to_string(),
            command: config.command.clone(),
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

impl Agent for VibeAgent {
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
            ..Default::default()
        }
    }

    fn agent() -> VibeAgent {
        VibeAgent::from_config("vibe", &AgentConfig::default())
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
    fn headless_paths_match_the_example_config() {
        let no_model = agent()
            .command(
                &Invocation::Headless {
                    prompt: "do it",
                    provider: None,
                    model: None,
                },
                &ctx(Some("do it"), None, None),
            )
            .unwrap();
        assert_eq!(no_model.args, vec!["-p", "do it", "--auto-approve"]);

        let with_model = agent()
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
            with_model.args,
            vec![
                "-p",
                "--provider",
                "mistral",
                "--model",
                "large",
                "do it",
                "--auto-approve"
            ]
        );
    }
}
