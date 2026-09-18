//! Skill loading: folders under `skills/` with an `AGENT.md` (role + prompt) and a
//! `config.toml` (model, MCP servers, tool allowlist, limits).
//!
//! A skill is the unit the agent runtime executes. Its config declares which MCP
//! servers to connect and which of their tools may be used — the per-skill
//! allowlist that gates tool access.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

/// A loaded skill: name, prompt text, and parsed config.
pub struct Skill {
    pub name: String,
    /// On-disk location of the skill folder.
    #[allow(dead_code)] // used once skills support relative paths / custom tools
    pub dir: PathBuf,
    /// Contents of `AGENT.md`.
    pub prompt: String,
    pub config: SkillConfig,
}

/// Normalized MCP server config derived from `config.toml`'s `[mcp.<name>]` table.
pub struct McpServerConfig {
    pub name: String,
    pub transport: McpTransport,
}

pub enum McpTransport {
    /// Spawn a child process (e.g. `mcp-filesystem --root ./workdir`).
    Stdio { command: String, args: Vec<String> },
    /// Connect to a remote MCP server. Deferred to M3+.
    StreamableHttp {
        #[allow(dead_code)] // consumed once the HTTP transport lands in M3
        url: String,
    },
}

#[derive(Debug, Default, Deserialize)]
pub struct SkillConfig {
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub mcp: BTreeMap<String, McpServerToml>,
    #[serde(default)]
    pub tools: ToolsConfig,
}

#[derive(Debug, Default, Deserialize)]
pub struct AgentConfig {
    /// Backend selector: `"mock"` uses the scripted [`MockStep`]s below; an
    /// `"openai:<model>"` value selects the (feature-gated) OpenAI backend.
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_max_iterations")]
    pub max_iterations: usize,
    /// Scripted steps consumed by `MockBackend`. Ignored by real backends.
    #[serde(default)]
    pub steps: Vec<MockStep>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpServerToml {
    pub transport: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ToolsConfig {
    /// `[tools.allow]` — server name → allowed tool names.
    #[serde(default)]
    pub allow: BTreeMap<String, Vec<String>>,
}

/// One scripted step for the mock model backend.
#[derive(Debug, Clone, Deserialize)]
pub struct MockStep {
    /// `"tool"` or `"final"`.
    pub kind: String,
    #[serde(default)]
    pub server: Option<String>,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub args: Option<serde_json::Value>,
    #[serde(default)]
    pub text: Option<String>,
}

fn default_model() -> String {
    "mock".to_string()
}

fn default_max_iterations() -> usize {
    20
}

/// Load every skill under `root` (one subdirectory each).
pub fn load_skills(root: &Path) -> anyhow::Result<Vec<Skill>> {
    let mut skills = Vec::new();
    let entries = std::fs::read_dir(root)
        .with_context(|| format!("skills dir {} not found", root.display()))?;

    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let dir = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();

        let prompt = std::fs::read_to_string(dir.join("AGENT.md")).with_context(|| {
            format!("skill {name}: missing AGENT.md in {}", dir.display())
        })?;
        let config_raw = std::fs::read_to_string(dir.join("config.toml")).with_context(|| {
            format!("skill {name}: missing config.toml in {}", dir.display())
        })?;
        let config: SkillConfig = toml::from_str(&config_raw)
            .with_context(|| format!("skill {name}: invalid config.toml"))?;

        skills.push(Skill {
            name,
            dir,
            prompt,
            config,
        });
    }

    skills.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(skills)
}

impl Skill {
    /// Resolve the `[mcp.*]` tables into normalized server configs.
    pub fn mcp_servers(&self) -> anyhow::Result<Vec<McpServerConfig>> {
        self.config
            .mcp
            .iter()
            .map(|(name, toml)| server_config(&self.name, name, toml))
            .collect()
    }
}

/// Convert an `[mcp.<name>]` table into a normalized [`McpServerConfig`].
///
/// Shared by the per-skill config and the global `~/.config/favetto` config, so both
/// declare MCP servers with the same shape.
pub fn server_config(
    owner: &str,
    name: &str,
    toml: &McpServerToml,
) -> anyhow::Result<McpServerConfig> {
    match toml.transport.as_str() {
        "stdio" => {
            let command = toml
                .command
                .clone()
                .with_context(|| format!("{owner}: mcp.{name} missing 'command'"))?;
            Ok(McpServerConfig {
                name: name.to_string(),
                transport: McpTransport::Stdio {
                    command,
                    args: toml.args.clone().unwrap_or_default(),
                },
            })
        }
        "streamable_http" => {
            let url = toml
                .url
                .clone()
                .with_context(|| format!("{owner}: mcp.{name} missing 'url'"))?;
            Ok(McpServerConfig {
                name: name.to_string(),
                transport: McpTransport::StreamableHttp { url },
            })
        }
        other => anyhow::bail!("{owner}: mcp.{name} has unknown transport {other:?}"),
    }
}
