//! Global configuration for favetto, loaded from `~/.config/favetto/config.toml`.
//!
//! This is where operators declare LLM **providers**, **global MCP servers**, and
//! daemon defaults — the same MCP shape used by skills' `config.toml`. The file is
//! parsed with the `config` crate (layered: file, then `FAVETTO_*` environment
//! variables), and located via the `dirs` crate (`dirs::config_dir()`).
//!
//! ```toml
//! # ~/.config/favetto/config.toml
//! [agent]
//! model = "echo"            # or "openai:gpt-4o"
//!
//! [providers.openai]
//! kind = "openai"
//! api_key_env = "OPENAI_API_KEY"   # or api_key = "..."
//! model = "gpt-4o"
//!
//! [mcp.filesystem]
//! transport = "stdio"
//! command = "mcp-filesystem"
//! args = ["--root", "./workdir"]
//!
//! [daemon]
//! listen = "127.0.0.1:7878"
//! socket = "/tmp/favetto.sock"
//! skills_dir = "skills"
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::skills::{self, McpServerConfig, McpServerToml};

#[derive(Debug, Clone, Default, Deserialize)]
pub struct FavettoConfig {
    /// Default model for the interactive chat / runtime.
    #[serde(default)]
    pub agent: AgentSettings,
    /// LLM providers by name (e.g. `openai`). Read by the feature-gated OpenAI
    /// backend; harmless to declare without it.
    #[serde(default)]
    #[allow(dead_code)]
    pub providers: BTreeMap<String, Provider>,
    /// Global MCP servers, merged under per-skill servers.
    #[serde(default)]
    pub mcp: BTreeMap<String, McpServerToml>,
    /// Daemon defaults (overridable by CLI flags).
    #[serde(default)]
    pub daemon: DaemonSettings,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AgentSettings {
    pub model: Option<String>,
}

/// An LLM provider. Only `kind = "openai"` is wired so far (feature-gated).
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct Provider {
    pub kind: String,
    #[serde(default)]
    pub api_key: Option<String>,
    /// Read the key from this environment variable instead of `api_key`.
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DaemonSettings {
    pub listen: Option<String>,
    pub socket: Option<PathBuf>,
    pub skills_dir: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
}

impl FavettoConfig {
    /// Load from `path`, tolerating a missing file.
    pub fn load_from(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let settings = config::Config::builder()
            .add_source(config::File::from(path))
            .add_source(config::Environment::with_prefix("FAVETTO").separator("__"))
            .build()?;
        Ok(settings.try_deserialize()?)
    }

    /// Global MCP servers as normalized configs (skill-level servers override these).
    pub fn mcp_servers(&self) -> anyhow::Result<Vec<McpServerConfig>> {
        self.mcp
            .iter()
            .map(|(name, toml)| skills::server_config("config", name, toml))
            .collect()
    }

    /// The first provider of `kind` (used for backend construction).
    #[allow(dead_code)] // read by the feature-gated openai backend
    pub fn provider(&self, kind: &str) -> Option<(&str, &Provider)> {
        self.providers
            .iter()
            .find(|(_, p)| p.kind == kind)
            .map(|(name, p)| (name.as_str(), p))
    }
}
