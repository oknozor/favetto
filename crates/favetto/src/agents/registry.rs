//! The configured-agent registry.
//!
//! Maps every `[agents.<name>]` entry to an implementation. The optional
//! `type = "…"` discriminator selects a built-in explicitly; without it, an
//! entry whose name is a built-in (`opencode`, `claude`, `pi`, `vibe`) uses that
//! built-in, and anything else is a template-only
//! [`ConfigurableAgent`](super::configurable::ConfigurableAgent).

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::config::FavettoConfig;

use super::agent::{Agent, AgentDescriptor};
use super::claude::ClaudeAgent;
use super::configurable::ConfigurableAgent;
use super::opencode::OpenCodeAgent;
use super::pi::PiAgent;
use super::vibe::VibeAgent;

/// Every configured agent, keyed by its `[agents.*]` name.
#[derive(Default)]
pub struct AgentRegistry {
    default: Option<String>,
    agents: BTreeMap<String, Arc<dyn Agent>>,
}

impl AgentRegistry {
    /// Build the registry from a loaded config. Unknown `type` values are an
    /// error so a typo fails at daemon startup rather than at launch time.
    ///
    /// The registry is built once: the daemon never rewrites the config today. If
    /// the config becomes hot-reloadable, rebuild it then.
    pub fn from_config(config: &FavettoConfig) -> anyhow::Result<Self> {
        let mut agents: BTreeMap<String, Arc<dyn Agent>> = BTreeMap::new();
        for (name, agent_config) in &config.agents {
            let agent: Arc<dyn Agent> = match agent_config.agent_type.as_deref() {
                Some("configurable") => {
                    Arc::new(ConfigurableAgent::from_config(name, agent_config))
                }
                Some("opencode") => Arc::new(OpenCodeAgent::from_config(name, agent_config)),
                Some("claude") => Arc::new(ClaudeAgent::from_config(name, agent_config)),
                Some("pi") => Arc::new(PiAgent::from_config(name, agent_config)),
                Some("vibe") => Arc::new(VibeAgent::from_config(name, agent_config)),
                Some(other) => anyhow::bail!(
                    "agent '{name}': unknown type '{other}' (known types: opencode, claude, \
                     pi, vibe, configurable)"
                ),
                None => match name.as_str() {
                    "opencode" => Arc::new(OpenCodeAgent::from_config(name, agent_config)),
                    "claude" => Arc::new(ClaudeAgent::from_config(name, agent_config)),
                    "pi" => Arc::new(PiAgent::from_config(name, agent_config)),
                    "vibe" => Arc::new(VibeAgent::from_config(name, agent_config)),
                    _ => Arc::new(ConfigurableAgent::from_config(name, agent_config)),
                },
            };
            agents.insert(name.clone(), agent);
        }
        Ok(Self {
            default: config.agent.default.clone(),
            agents,
        })
    }

    /// The agent configured under `name`, if any.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Agent>> {
        self.agents.get(name).cloned()
    }

    /// The configured default agent, if it resolves to an entry.
    pub fn default_agent(&self) -> Option<Arc<dyn Agent>> {
        self.default.as_deref().and_then(|name| self.get(name))
    }

    /// Every descriptor, sorted by name (the `BTreeMap` key order).
    pub fn descriptors(&self) -> Vec<&AgentDescriptor> {
        self.agents
            .values()
            .map(|agent| agent.descriptor())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::agent::{AgentContext, Invocation};
    use super::*;
    use crate::config::AgentConfig;

    fn config_with(name: &str, agent: AgentConfig) -> FavettoConfig {
        let mut cfg = FavettoConfig::default();
        cfg.agents.insert(name.to_string(), agent);
        cfg
    }

    #[test]
    fn built_in_name_uses_the_built_in() {
        let cfg = config_with("opencode", AgentConfig::default());
        let registry = AgentRegistry::from_config(&cfg).unwrap();
        let agent = registry.get("opencode").unwrap();
        assert_eq!(agent.descriptor().name, "OpenCode");
        assert!(agent.capabilities().providers);
    }

    #[test]
    fn unknown_name_uses_configurable_agent() {
        let cfg = config_with(
            "mycli",
            AgentConfig {
                command: "mycli".to_string(),
                ..Default::default()
            },
        );
        let registry = AgentRegistry::from_config(&cfg).unwrap();
        let agent = registry.get("mycli").unwrap();
        assert_eq!(agent.descriptor().name, "mycli");
        assert!(!agent.capabilities().providers);
    }

    #[test]
    fn explicit_type_on_custom_name_selects_the_built_in() {
        let cfg = config_with(
            "fast-claude",
            AgentConfig {
                agent_type: Some("claude".to_string()),
                ..Default::default()
            },
        );
        let registry = AgentRegistry::from_config(&cfg).unwrap();
        assert_eq!(
            registry.get("fast-claude").unwrap().descriptor().name,
            "Claude"
        );
    }

    #[test]
    fn explicit_configurable_type_opts_out_of_built_in() {
        let cfg = config_with(
            "opencode",
            AgentConfig {
                agent_type: Some("configurable".to_string()),
                command: "opencode".to_string(),
                ..Default::default()
            },
        );
        let registry = AgentRegistry::from_config(&cfg).unwrap();
        assert_eq!(
            registry.get("opencode").unwrap().descriptor().name,
            "opencode"
        );
        assert!(!registry.get("opencode").unwrap().capabilities().providers);
    }

    #[test]
    fn unknown_type_is_an_error() {
        let cfg = config_with(
            "mystery",
            AgentConfig {
                agent_type: Some("gemini".to_string()),
                ..Default::default()
            },
        );
        let err = AgentRegistry::from_config(&cfg).map(|_| ()).unwrap_err();
        assert!(err.to_string().contains("unknown type 'gemini'"));
    }

    #[test]
    fn config_overrides_win_over_built_in_defaults() {
        let cfg = config_with(
            "opencode",
            AgentConfig {
                command: "myoc".to_string(),
                headless_args: Some(vec!["custom".to_string(), "{prompt}".to_string()]),
                ..Default::default()
            },
        );
        let registry = AgentRegistry::from_config(&cfg).unwrap();
        let agent = registry.get("opencode").unwrap();
        assert_eq!(agent.descriptor().command, "myoc");
        let spec = agent
            .command(
                &Invocation::Headless {
                    prompt: "hi",
                    provider: None,
                    model: None,
                },
                &AgentContext {
                    prompt: Some("hi".to_string()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(spec.program, std::path::PathBuf::from("myoc"));
        assert_eq!(spec.args, vec!["custom", "hi"]);
    }

    #[test]
    fn descriptors_are_sorted_and_default_resolves() {
        let mut cfg = FavettoConfig::default();
        cfg.agents.insert(
            "vibe".to_string(),
            AgentConfig {
                command: "vibe".to_string(),
                ..Default::default()
            },
        );
        cfg.agents.insert(
            "opencode".to_string(),
            AgentConfig {
                command: "opencode".to_string(),
                ..Default::default()
            },
        );
        cfg.agent.default = Some("opencode".to_string());
        let registry = AgentRegistry::from_config(&cfg).unwrap();
        let names: Vec<&str> = registry
            .descriptors()
            .iter()
            .map(|d| d.id.as_str())
            .collect();
        assert_eq!(names, vec!["opencode", "vibe"]);
        assert_eq!(
            registry.default_agent().map(|a| a.descriptor().id.clone()),
            Some("opencode".to_string())
        );
    }
}
