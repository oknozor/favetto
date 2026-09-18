//! Global configuration for favetto, loaded from `~/.config/favetto/config.toml`.
//!
//! This is where operators declare LLM **providers**, **global MCP servers**, and
//! daemon defaults. The file is parsed with the `config` crate (layered: file, then
//! `FAVETTO_*` environment variables), and located via the `dirs` crate
//! (`dirs::config_dir()`).
//!
//! ```toml
//! # ~/.config/favetto/config.toml
//! [agent]
//! model = "deepseek:deepseek-v4-flash"   # or "echo"
//!
//! [providers.deepseek]
//! kind = "openai"
//! api_key_env = "DEEPSEEK_API_KEY"   # or api_key = "..."
//! model = "deepseek-v4-flash"
//! base_url = "https://api.deepseek.com"
//!
//! [mcp.filesystem]
//! transport = "stdio"
//! command = "mcp-filesystem"
//! args = ["--root", "./workdir"]
//!
//! [daemon]
//! listen = "127.0.0.1:7878"
//! socket = "/tmp/favetto.sock"
//! tasks_dir = "tasks"
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::mcp::{self, McpServerConfig, McpServerToml};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FavettoConfig {
    /// Default model for the interactive chat / runtime.
    #[serde(default)]
    pub agent: AgentSettings,
    /// LLM providers by name (e.g. `openai`, `deepseek`).
    #[serde(default)]
    pub providers: BTreeMap<String, Provider>,
    /// Global MCP servers available to task prompts.
    #[serde(default)]
    pub mcp: BTreeMap<String, McpServerToml>,
    /// Daemon defaults (overridable by CLI flags).
    #[serde(default)]
    pub daemon: DaemonSettings,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentSettings {
    pub model: Option<String>,
}

/// An LLM provider. Any `kind = "openai"` provider is OpenAI-API-compatible.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Read the key from this environment variable instead of `api_key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DaemonSettings {
    pub listen: Option<String>,
    pub socket: Option<PathBuf>,
    pub tasks_dir: Option<PathBuf>,
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

    /// Global MCP servers as normalized configs (task prompts may use these).
    pub fn mcp_servers(&self) -> anyhow::Result<Vec<McpServerConfig>> {
        self.mcp
            .iter()
            .map(|(name, toml)| mcp::server_config("config", name, toml))
            .collect()
    }

    /// Serialize back to TOML (used to persist runtime provider changes).
    pub fn to_toml(&self) -> anyhow::Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }
}
