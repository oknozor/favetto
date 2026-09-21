//! Built-in agent for [pi](https://github.com/badlogic/pi-mono).

use crate::config::AgentConfig;

use super::agent::{Agent, AgentContext, AgentDescriptor, CommandSpec, Invocation};
use super::configurable::{capabilities_from_config, overlay, TemplateAgent};

/// The built-in defaults, matching `config.example.toml`.
fn base_config() -> AgentConfig {
    AgentConfig {
        command: "pi".to_string(),
        args: Vec::new(),
        ..Default::default()
    }
}

/// The pi implementation: interactive-only in the shipped defaults.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent() -> PiAgent {
        PiAgent::from_config("pi", &AgentConfig::default())
    }

    #[test]
    fn only_interactive_is_enabled() {
        let caps = agent().capabilities();
        assert!(caps.interactive);
        assert!(!caps.headless);
        assert!(!caps.resume);
        assert!(!caps.model_selection);
        assert!(!caps.providers);
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
                &AgentContext::default(),
            )
            .unwrap();
        assert_eq!(spec.program, std::path::PathBuf::from("pi"));
        assert!(spec.args.is_empty());
    }

    #[test]
    fn headless_without_args_is_rejected() {
        let err = agent()
            .command(
                &Invocation::Headless {
                    prompt: "hi",
                    provider: None,
                    model: None,
                },
                &AgentContext::default(),
            )
            .unwrap_err();
        assert!(err.to_string().contains("headless_args"));
    }
}
