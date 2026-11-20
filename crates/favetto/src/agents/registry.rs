//! The configured-agent registry.
//!
//! Every agent is an ACP server. The four built-in names are **launch presets**
//! (command + arguments for a known ACP server), not implementations: a
//! `[agents.<name>]` entry overlays the preset, and any other name is a plain
//! ACP configuration. Adding an agent is configuration, not code.

use std::collections::BTreeMap;
use std::env;
use std::path::Path;
use std::sync::Arc;

use crate::config::{AgentConfig, FavettoConfig};

use super::Agent;

/// The built-in ACP launch presets: `(name, command, args)`.
const BUILT_IN_AGENTS: &[(&str, &str, &[&str])] = &[
    ("opencode", "opencode", &["acp"]),
    ("claude", "npx", &["-y", "@zed-industries/claude-agent-acp"]),
    ("pi", "npx", &["-y", "pi-acp"]),
    ("vibe", "vibe-acp", &[]),
];

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

/// A built-in launch preset, or `None` for a name with no preset.
fn preset(name: &str) -> Option<AgentConfig> {
    BUILT_IN_AGENTS
        .iter()
        .find(|(preset_name, _, _)| *preset_name == name)
        .map(|(_, command, args)| AgentConfig {
            command: (*command).to_string(),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            ..Default::default()
        })
}

/// Overlay the non-default fields of `overrides` onto `base`.
fn overlay(base: &mut AgentConfig, overrides: &AgentConfig) {
    if !overrides.command.trim().is_empty() {
        base.command = overrides.command.clone();
    }
    if !overrides.args.is_empty() {
        base.args = overrides.args.clone();
    }
    if !overrides.env.is_empty() {
        for (key, value) in &overrides.env {
            base.env.insert(key.clone(), value.clone());
        }
    }
    if let Some(cwd) = &overrides.cwd {
        base.cwd = Some(cwd.clone());
    }
    if let Some(permission) = overrides.permission {
        base.permission = Some(permission);
    }
    if let Some(git) = &overrides.git {
        base.git = Some(git.clone());
    }
}

/// Every configured agent, keyed by its `[agents.*]` name.
pub struct AgentRegistry {
    default: Option<String>,
    agents: BTreeMap<String, Arc<Agent>>,
}

impl Default for AgentRegistry {
    #[allow(
        clippy::expect_used,
        reason = "the default config only contains built-in presets, which always build"
    )]
    fn default() -> Self {
        Self::from_config_with(&FavettoConfig::default(), &|_| true)
            .expect("built-in-only registry always builds")
    }
}

impl AgentRegistry {
    /// Build the registry from a loaded config, probing the host `PATH` for each
    /// agent's executable.
    pub fn from_config(config: &FavettoConfig) -> anyhow::Result<Self> {
        Self::from_config_with(config, &command_available)
    }

    /// Like [`Self::from_config`], but with an injectable availability probe.
    pub fn from_config_with(
        config: &FavettoConfig,
        probe: &dyn Fn(&str) -> bool,
    ) -> anyhow::Result<Self> {
        let mut agents: BTreeMap<String, Arc<Agent>> = BTreeMap::new();

        for (name, _, _) in BUILT_IN_AGENTS {
            if !config.agents.contains_key(*name) {
                if let Some(config) = preset(name) {
                    insert_agent(&mut agents, name, config, probe);
                }
            }
        }

        for (name, agent_config) in &config.agents {
            let mut resolved = preset(name).unwrap_or_default();
            overlay(&mut resolved, agent_config);
            insert_agent(&mut agents, name, resolved, probe);
        }

        Ok(Self {
            default: config.agent.default.clone(),
            agents,
        })
    }

    /// The agent configured under `name`, if any.
    pub fn get(&self, name: &str) -> Option<Arc<Agent>> {
        self.agents.get(name).cloned()
    }

    /// The agent configured under `name`, erroring if it is unknown or its
    /// executable is not on the daemon's `PATH`.
    pub fn get_checked(&self, name: &str) -> anyhow::Result<Arc<Agent>> {
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
    pub fn default_agent(&self) -> Option<Arc<Agent>> {
        self.default.as_deref().and_then(|name| self.get(name))
    }

    /// Every descriptor, sorted by name (the `BTreeMap` key order).
    pub fn descriptors(&self) -> Vec<&super::AgentDescriptor> {
        self.agents
            .values()
            .map(|agent| agent.descriptor())
            .collect()
    }
}

/// Probe, stamp and insert one agent.
fn insert_agent(
    agents: &mut BTreeMap<String, Arc<Agent>>,
    name: &str,
    config: AgentConfig,
    probe: &dyn Fn(&str) -> bool,
) {
    let mut agent = Agent::from_config(name, &config);
    let available = probe(&agent.descriptor().command);
    agent.set_available(available);
    agents.insert(name.to_string(), Arc::new(agent));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_ins_are_acp_presets() {
        let registry =
            AgentRegistry::from_config_with(&FavettoConfig::default(), &|_| true).unwrap();
        let names: Vec<&str> = registry
            .descriptors()
            .iter()
            .map(|d| d.id.as_str())
            .collect();
        assert_eq!(names, vec!["claude", "opencode", "pi", "vibe"]);

        let opencode = registry.get("opencode").unwrap();
        assert!(opencode.descriptor().capabilities.acp);
        assert_eq!(opencode.descriptor().command, "opencode");
    }

    #[test]
    fn a_config_entry_overlays_the_preset() {
        let mut config = FavettoConfig::default();
        config.agents.insert(
            "opencode".to_string(),
            AgentConfig {
                command: "my-opencode".to_string(),
                ..Default::default()
            },
        );
        let registry = AgentRegistry::from_config_with(&config, &|_| true).unwrap();
        let agent = registry.get("opencode").unwrap();
        assert_eq!(agent.descriptor().command, "my-opencode");
        // The preset's arguments survive a command-only override.
        assert!(agent.descriptor().capabilities.acp);
    }

    #[test]
    fn an_unknown_name_is_a_plain_acp_agent() {
        let mut config = FavettoConfig::default();
        config.agents.insert(
            "goose".to_string(),
            AgentConfig {
                command: "goose".to_string(),
                args: vec!["acp".to_string()],
                ..Default::default()
            },
        );
        let registry = AgentRegistry::from_config_with(&config, &|_| true).unwrap();
        assert_eq!(registry.get("goose").unwrap().descriptor().command, "goose");
    }

    #[test]
    fn the_probe_sets_availability() {
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
        assert!(registry.get_checked("claude").is_err());
        assert!(registry.get_checked("opencode").is_ok());
    }
}
