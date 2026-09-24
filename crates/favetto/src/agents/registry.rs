//! The configured-agent registry.
//!
//! The four compiled-in agents (`opencode`, `claude`, `pi`, `vibe`) are always
//! registered as a baseline, so an empty/default config still exposes them.
//! `[agents.<name>]` entries are an overlay: the optional `type = "…"`
//! discriminator selects a built-in explicitly; without it, an entry whose name
//! is a built-in uses that built-in, and anything else is a template-only
//! [`ConfigurableAgent`](super::configurable::ConfigurableAgent). Each agent's
//! availability is probed once, at registry build.

use std::collections::BTreeMap;
use std::env;
use std::path::Path;
use std::sync::Arc;

use crate::config::{AgentConfig, FavettoConfig};

use super::agent::{Agent, AgentDescriptor};
use super::claude::ClaudeAgent;
use super::configurable::ConfigurableAgent;
use super::opencode::OpenCodeAgent;
use super::pi::PiAgent;
use super::vibe::VibeAgent;

/// The compiled-in agents, always registered unless their name is overridden by
/// a `[agents.<name>]` entry.
const BUILT_IN_AGENTS: [&str; 4] = ["opencode", "claude", "pi", "vibe"];

/// Whether `command` names a runnable executable: an executable file when it
/// contains a path separator (absolute or relative), else a `PATH` lookup.
pub fn command_available(command: &str) -> bool {
    let command = command.trim();
    if command.is_empty() {
        return false;
    }
    let path = Path::new(command);
    if path.components().count() > 1 {
        return is_executable(path);
    }
    let Some(paths) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&paths).any(|dir| is_executable(&dir.join(command)))
}

/// Whether `path` is a file the current user may execute.
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Build one agent from its `[agents.<name>]` entry (or a built-in's defaults).
fn build_agent(name: &str, agent_config: &AgentConfig) -> anyhow::Result<Box<dyn Agent>> {
    let agent: Box<dyn Agent> = match agent_config.agent_type.as_deref() {
        Some("configurable") => Box::new(ConfigurableAgent::from_config(name, agent_config)),
        Some("opencode") => Box::new(OpenCodeAgent::from_config(name, agent_config)),
        Some("claude") => Box::new(ClaudeAgent::from_config(name, agent_config)),
        Some("pi") => Box::new(PiAgent::from_config(name, agent_config)),
        Some("vibe") => Box::new(VibeAgent::from_config(name, agent_config)),
        Some(other) => anyhow::bail!(
            "agent '{name}': unknown type '{other}' (known types: opencode, claude, \
             pi, vibe, configurable)"
        ),
        None => match name {
            "opencode" => Box::new(OpenCodeAgent::from_config(name, agent_config)),
            "claude" => Box::new(ClaudeAgent::from_config(name, agent_config)),
            "pi" => Box::new(PiAgent::from_config(name, agent_config)),
            "vibe" => Box::new(VibeAgent::from_config(name, agent_config)),
            _ => Box::new(ConfigurableAgent::from_config(name, agent_config)),
        },
    };
    Ok(agent)
}

/// Every configured agent, keyed by its `[agents.*]` name.
pub struct AgentRegistry {
    default: Option<String>,
    agents: BTreeMap<String, Arc<dyn Agent>>,
}

impl Default for AgentRegistry {
    #[allow(
        clippy::expect_used,
        reason = "the default FavettoConfig configures only built-in agents, which always construct"
    )]
    fn default() -> Self {
        Self::from_config(&FavettoConfig::default()).expect("built-in-only registry always builds")
    }
}

impl AgentRegistry {
    /// Build the registry from a loaded config, probing the host `PATH` for each
    /// agent's executable. Unknown `type` values are an error so a typo fails at
    /// daemon startup rather than at launch time.
    ///
    /// The registry is built once: the daemon never rewrites the config today. If
    /// the config becomes hot-reloadable, rebuild it then.
    pub fn from_config(config: &FavettoConfig) -> anyhow::Result<Self> {
        Self::from_config_with(config, &command_available)
    }

    /// Like [`Self::from_config`], but with an injectable availability probe.
    /// `probe` decides whether an agent's `command` is runnable (used by tests
    /// so results do not depend on the host's installed CLIs).
    pub fn from_config_with(
        config: &FavettoConfig,
        probe: &dyn Fn(&str) -> bool,
    ) -> anyhow::Result<Self> {
        let mut agents: BTreeMap<String, Arc<dyn Agent>> = BTreeMap::new();

        // Seed the compiled-in agents first; a `[agents.<name>]` entry overlays
        // the matching built-in below.
        for name in BUILT_IN_AGENTS {
            if !config.agents.contains_key(name) {
                let agent = build_agent(name, &AgentConfig::default())?;
                insert_agent(&mut agents, name, agent, probe);
            }
        }

        for (name, agent_config) in &config.agents {
            let agent = build_agent(name, agent_config)?;
            insert_agent(&mut agents, name, agent, probe);
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

    /// The agent configured under `name`, erroring if it is unknown or its
    /// executable is not on the daemon's `PATH`.
    pub fn get_checked(&self, name: &str) -> anyhow::Result<Arc<dyn Agent>> {
        let agent = self
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("agent '{name}' is not configured under [agents.*]"))?;
        if !agent.descriptor().available {
            let command = &agent.descriptor().command;
            anyhow::bail!(
                "agent '{name}' is not installed (command '{command}' not found on PATH)"
            );
        }
        Ok(agent)
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

/// Probe, stamp and insert one built agent.
fn insert_agent(
    agents: &mut BTreeMap<String, Arc<dyn Agent>>,
    name: &str,
    mut agent: Box<dyn Agent>,
    probe: &dyn Fn(&str) -> bool,
) {
    let available = probe(agent.descriptor().command.as_str());
    agent.set_available(available);
    agents.insert(name.to_string(), Arc::from(agent));
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
        // The built-ins are always seeded, so the configured entries join them.
        assert_eq!(names, vec!["claude", "opencode", "pi", "vibe"]);
        assert_eq!(
            registry.default_agent().map(|a| a.descriptor().id.clone()),
            Some("opencode".to_string())
        );
    }

    #[test]
    fn empty_config_registers_all_four_built_ins() {
        let registry =
            AgentRegistry::from_config_with(&FavettoConfig::default(), &|_| true).unwrap();
        let names: Vec<&str> = registry
            .descriptors()
            .iter()
            .map(|d| d.id.as_str())
            .collect();
        assert_eq!(names, vec!["claude", "opencode", "pi", "vibe"]);

        let opencode = registry.get("opencode").unwrap();
        assert_eq!(opencode.descriptor().name, "OpenCode");
        assert!(opencode.capabilities().providers);
    }

    #[test]
    fn config_entry_overlays_built_in() {
        let cfg = config_with(
            "opencode",
            AgentConfig {
                command: "myoc".to_string(),
                ..Default::default()
            },
        );
        let registry = AgentRegistry::from_config_with(&cfg, &|_| true).unwrap();
        let agent = registry.get("opencode").unwrap();
        // The OpenCode implementation is retained; only its invocation is overridden.
        assert_eq!(agent.descriptor().command, "myoc");
        assert!(agent.capabilities().providers);
    }

    #[test]
    fn injected_probe_sets_available_per_agent() {
        let registry =
            AgentRegistry::from_config_with(&FavettoConfig::default(), &|cmd| cmd == "opencode")
                .unwrap();
        let flags: Vec<(&str, bool)> = registry
            .descriptors()
            .iter()
            .map(|d| (d.id.as_str(), d.available))
            .collect();
        assert_eq!(
            flags,
            vec![
                ("claude", false),
                ("opencode", true),
                ("pi", false),
                ("vibe", false)
            ]
        );
    }

    #[test]
    fn get_checked_rejects_unavailable() {
        let registry =
            AgentRegistry::from_config_with(&FavettoConfig::default(), &|cmd| cmd == "opencode")
                .unwrap();
        let err = registry
            .get_checked("claude")
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(err.contains("is not installed"), "{err}");
        assert!(err.contains("command 'claude'"), "{err}");
        assert!(registry.get_checked("opencode").is_ok());
    }

    #[test]
    fn get_checked_rejects_unknown() {
        let registry =
            AgentRegistry::from_config_with(&FavettoConfig::default(), &|_| true).unwrap();
        let err = registry
            .get_checked("nope")
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(err.contains("is not configured under [agents.*]"), "{err}");
    }

    #[test]
    fn command_available_checks_absolute_paths() {
        assert!(!command_available(""));
        assert!(!command_available("   "));
        assert!(!command_available("/nonexistent/favetto-nope"));
        let current = std::env::current_exe().unwrap();
        assert!(command_available(&current.to_string_lossy()));
    }
}
