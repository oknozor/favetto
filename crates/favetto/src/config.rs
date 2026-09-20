//! Global configuration for favetto, loaded from `~/.config/favetto/config.toml`.
//!
//! This is where operators declare the **external agents** and daemon defaults.
//! The file is parsed with the `config` crate (layered: file, then
//! `FAVETTO_*` environment variables), and located via the `dirs` crate
//! (`dirs::config_dir()`).
//!
//! ```toml
//! # ~/.config/favetto/config.toml
//! [agent]
//! default = "opencode"                   # default external agent
//!
//! [agents.opencode]                      # external coding agents
//! command = "opencode"
//! headless_args = ["run", "{prompt}"]    # unattended catalog-task runs
//!
//! [daemon]
//! listen = "127.0.0.1:7878"
//! socket = "/tmp/favetto.sock"
//! tasks_dir = "tasks"
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FavettoConfig {
    /// Default external agent used for catalog tasks and new agent sessions.
    #[serde(default)]
    pub agent: AgentSettings,
    /// External coding agents by name (e.g. `claude`, `opencode`, `pi`, `vibe`).
    #[serde(default)]
    pub agents: BTreeMap<String, AgentConfig>,
    /// Daemon defaults (overridable by CLI flags).
    #[serde(default)]
    pub daemon: DaemonSettings,
    /// Task executor concurrency / isolation.
    #[serde(default)]
    pub executor: ExecutorSettings,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentSettings {
    /// Name of the default external agent (a key in `[agents.*]`). When set,
    /// catalog tasks run through this agent unless a task names its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// An external coding-agent CLI (opencode, Claude Code, pi, Mistral Vibe, …).
///
/// `args` is the interactive invocation used by the embedded terminal panel.
/// `prompt_args`, when present, is appended (with `{prompt}` substituted) so a
/// session can start with the task prompt. `headless_args` drives unattended
/// catalog-task runs; `{prompt}` is substituted there too. When a prompt is given
/// but no `prompt_args`/`headless_args` are configured, the prompt is written to
/// the process's stdin.
///
/// Model selection uses `run_args` instead of `headless_args` when a task sets a
/// `model`. Both support `{prompt}`, `{provider}`, and `{model}` placeholders, so
/// the flag can be placed where the CLI expects it (e.g. after a `run`
/// subcommand). `resume_args` is the interactive invocation for reopening a
/// previously run session and supports `{session_id}`. When `session_id_json_key`
/// is set, the agent's session id is read from a run's line-delimited JSON output
/// under that key (e.g. `"sessionID"`) and stored on the task.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Executable to launch (looked up on `PATH`).
    pub command: String,
    /// Interactive arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Arguments for unattended task runs without a `model`. `{prompt}` is substituted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headless_args: Option<Vec<String>>,
    /// Arguments for unattended task runs with a `model`; `{prompt}`, `{provider}`,
    /// and `{model}` are substituted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_args: Option<Vec<String>>,
    /// Interactive arguments for reopening an existing session; `{session_id}` is
    /// substituted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_args: Option<Vec<String>>,
    /// Key under which a run's line-delimited JSON output carries the agent's
    /// session id (e.g. `"sessionID"`). Unset disables capture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id_json_key: Option<String>,
    /// Arguments appended to the interactive command when starting with a prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_args: Option<Vec<String>>,
    /// After starting interactively with a prompt via `prompt_args`, send Enter to
    /// submit it once the agent's UI has settled (some agents, e.g. opencode,
    /// only pre-fill the input with `--prompt`).
    #[serde(default)]
    pub submit_prompt: bool,
    /// Extra environment variables for the process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Working directory (defaults to the daemon's cwd).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DaemonSettings {
    pub listen: Option<String>,
    pub socket: Option<PathBuf>,
    pub tasks_dir: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
}

fn default_true() -> bool {
    true
}

fn default_max_concurrency() -> usize {
    4
}

/// How the executor runs tasks: concurrency and directory/worktree isolation.
///
/// With `parallel = true` each task gets a git worktree when its working
/// directory is inside a repository (so tasks can run side by side without
/// stepping on each other). Tasks that do **not** run in a worktree are
/// serialized per working directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutorSettings {
    /// Run multiple tasks at once (default false: one at a time).
    #[serde(default)]
    pub parallel: bool,
    /// Maximum concurrent tasks when `parallel` is true.
    #[serde(default = "default_max_concurrency")]
    pub max_concurrency: usize,
    /// Give each task its own `git worktree` when it runs in a repository.
    #[serde(default = "default_true")]
    pub worktree: bool,
    /// Where worktrees are created: absolute, or relative to the repo root.
    /// Defaults to `<data_dir>/worktrees`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_dir: Option<PathBuf>,
    /// Keep worktrees after the task finishes (default true) so the agent's
    /// branch/changes can be inspected; false removes them.
    #[serde(default = "default_true")]
    pub keep_worktree: bool,
}

impl Default for ExecutorSettings {
    fn default() -> Self {
        Self {
            parallel: false,
            max_concurrency: default_max_concurrency(),
            worktree: true,
            worktree_dir: None,
            keep_worktree: true,
        }
    }
}

impl ExecutorSettings {
    /// Effective concurrency (always at least 1).
    pub fn concurrency(&self) -> usize {
        if self.parallel {
            self.max_concurrency.max(1)
        } else {
            1
        }
    }
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
}
