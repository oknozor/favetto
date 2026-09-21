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
    /// Webhook trigger rules (currently GitHub).
    #[serde(default)]
    pub webhook: WebhookSettings,
    /// Client-side TUI settings. Ignored by the daemon, which reads the same file.
    #[serde(default)]
    pub tui: TuiSettings,
}

/// TUI-only settings. The daemon parses but ignores this section.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TuiSettings {
    #[serde(default)]
    pub sound: SoundSettings,
}

/// Client-side sound notifications for the TUI.
///
/// Precedence is CLI flags (`--sound`/`--no-sound`/`--sound-command`) over the
/// `FAVETTO_SOUND*` environment variables over this section over built-in
/// defaults. Sounds are played on the machine running `favetto tui`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoundSettings {
    /// Master switch (default true).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// `"auto"` (detect a player on `PATH`), `"bell"`, or `"command"`.
    #[serde(default = "default_sound_player")]
    pub player: String,
    /// Template used when `player = "command"`; `{file}` is the sound path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Directory for relative `.wav` event values. `~` is expanded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sound_dir: Option<PathBuf>,
    /// Minimum gap between cues; bursts inside it are coalesced (failure wins).
    #[serde(default = "default_min_interval_ms")]
    pub min_interval_ms: u64,
    /// Only play while the terminal is unfocused (best-effort focus reporting).
    #[serde(default)]
    pub only_when_unfocused: bool,
    /// Cue key -> sound spec (`success`/`failure`/`attention`/`started`/`bell`,
    /// a `.wav` path, or `none`). Unset keys use the built-in default.
    #[serde(default = "default_sound_events")]
    pub events: BTreeMap<String, String>,
}

impl Default for SoundSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            player: default_sound_player(),
            command: None,
            sound_dir: None,
            min_interval_ms: default_min_interval_ms(),
            only_when_unfocused: false,
            events: default_sound_events(),
        }
    }
}

/// Webhook trigger settings. Currently only GitHub is supported.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WebhookSettings {
    #[serde(default)]
    pub github: GithubWebhookSettings,
}

/// GitHub webhook receiver + trigger rules.
///
/// GitHub POSTs signed events to `/webhooks/github`; each matching rule enqueues
/// the named catalog task with a truncated summary of the event as its input.
/// Rules and the secret are read at daemon startup, so a restart is required after
/// editing them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GithubWebhookSettings {
    /// Opt-in; disabled unless set.
    #[serde(default)]
    pub enabled: bool,
    /// Name of the env var holding the signing secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_env: Option<String>,
    /// Literal secret (accepted but discouraged).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    #[serde(default)]
    pub rules: Vec<GithubRule>,
}

/// A single `[[webhook.github.rules]]` entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubRule {
    pub name: String,
    /// `X-GitHub-Event` value (e.g. `"issues"`). Validated at startup.
    pub event: String,
    /// Optional action; unset matches any action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// Catalog task name. Validated at startup.
    pub task: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub filter: GithubFilter,
}

/// Optional narrowing for a [`GithubRule`]. Unset fields match anything; all set
/// fields must match (AND).
///
/// `repo`, `author`, `base_ref`, and `head_ref` are globs; `labels_contains` is
/// any-of exact.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GithubFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels_contains: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_ref: Option<String>,
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
    /// Implementation discriminator: `opencode`, `claude`, `pi`, `vibe`, or
    /// `configurable`. Omit to use the built-in whose name matches the entry (or
    /// the template-only fallback for a custom name).
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
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
    /// Interactive arguments used when a model is selected (e.g. for a one-shot
    /// session); `{provider}` and `{model}` are substituted. Lets an agent reach a
    /// path that accepts a model when its plain `args` do not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interactive_model_args: Option<Vec<String>>,
    /// Key under which a run's line-delimited JSON output carries the agent's
    /// session id (e.g. `"sessionID"`). Unset disables capture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id_json_key: Option<String>,
    /// Arguments appended to the interactive command when starting with a prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_args: Option<Vec<String>>,
    /// After starting interactively with a prompt via `prompt_args`, send Enter to
    /// submit it once the agent's UI has settled (some agents, e.g. opencode,
    /// only pre-fill the input with `--prompt`). `None` leaves the built-in
    /// default in place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submit_prompt: Option<bool>,
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

fn default_sound_player() -> String {
    "auto".to_string()
}

fn default_min_interval_ms() -> u64 {
    400
}

fn default_sound_events() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("task_finished".to_string(), "success".to_string()),
        ("task_failed".to_string(), "failure".to_string()),
        ("task_started".to_string(), "none".to_string()),
        ("attention".to_string(), "none".to_string()),
    ])
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_webhook_github_rules() {
        let cfg: FavettoConfig = toml::from_str(
            r#"
            [webhook.github]
            enabled = true
            secret_env = "GITHUB_WEBHOOK_SECRET"

            [[webhook.github.rules]]
            name = "triage-opened-issues"
            event = "issues"
            action = "opened"
            task = "triage_favetto_issues"
            filter = { repo = "oknozor/*", labels_contains = ["bug"] }
            "#,
        )
        .unwrap();

        let gh = &cfg.webhook.github;
        assert!(gh.enabled);
        assert_eq!(gh.secret_env.as_deref(), Some("GITHUB_WEBHOOK_SECRET"));
        assert_eq!(gh.rules.len(), 1);
        let rule = &gh.rules[0];
        assert_eq!(rule.name, "triage-opened-issues");
        assert_eq!(rule.event, "issues");
        assert_eq!(rule.action.as_deref(), Some("opened"));
        assert_eq!(rule.task, "triage_favetto_issues");
        assert!(rule.enabled);
        assert_eq!(rule.filter.repo.as_deref(), Some("oknozor/*"));
        assert_eq!(rule.filter.labels_contains, vec!["bug".to_string()]);
    }

    #[test]
    fn parses_tui_sound_settings() {
        let cfg: FavettoConfig = toml::from_str(
            r#"
            [tui.sound]
            enabled = true
            player = "command"
            command = "paplay {file}"
            sound_dir = "/tmp/sounds"
            min_interval_ms = 250
            only_when_unfocused = true

            [tui.sound.events]
            task_finished = "success"
            task_failed = "bell"
            attention = "none"
            "#,
        )
        .unwrap();

        let sound = &cfg.tui.sound;
        assert!(sound.enabled);
        assert_eq!(sound.player, "command");
        assert_eq!(sound.command.as_deref(), Some("paplay {file}"));
        assert_eq!(sound.sound_dir.as_deref(), Some(Path::new("/tmp/sounds")));
        assert_eq!(sound.min_interval_ms, 250);
        assert!(sound.only_when_unfocused);
        assert_eq!(
            sound.events.get("task_finished").map(String::as_str),
            Some("success")
        );
        assert_eq!(
            sound.events.get("task_failed").map(String::as_str),
            Some("bell")
        );
        assert_eq!(
            sound.events.get("attention").map(String::as_str),
            Some("none")
        );
    }

    #[test]
    fn tui_sound_defaults() {
        let cfg: FavettoConfig = toml::from_str("").unwrap();
        let sound = &cfg.tui.sound;
        assert!(sound.enabled);
        assert_eq!(sound.player, "auto");
        assert_eq!(sound.min_interval_ms, 400);
        assert!(!sound.only_when_unfocused);
        assert_eq!(
            sound.events.get("task_finished").map(String::as_str),
            Some("success")
        );
        assert_eq!(
            sound.events.get("task_failed").map(String::as_str),
            Some("failure")
        );
        assert_eq!(
            sound.events.get("task_started").map(String::as_str),
            Some("none")
        );
        assert_eq!(
            sound.events.get("attention").map(String::as_str),
            Some("none")
        );
    }

    #[test]
    fn agent_config_type_and_submit_prompt_parse() {
        let cfg: FavettoConfig = toml::from_str(
            r#"
            [agents.opencode]
            type = "opencode"
            command = "opencode"
            submit_prompt = true

            [agents.custom]
            command = "custom"
            "#,
        )
        .unwrap();
        let opencode = &cfg.agents["opencode"];
        assert_eq!(opencode.agent_type.as_deref(), Some("opencode"));
        assert_eq!(opencode.submit_prompt, Some(true));

        // An absent `type` and `submit_prompt` stay unset.
        let custom = &cfg.agents["custom"];
        assert!(custom.agent_type.is_none());
        assert!(custom.submit_prompt.is_none());
    }

    #[test]
    fn agent_config_explicit_configurable_type_parses() {
        let cfg: FavettoConfig = toml::from_str(
            r#"
            [agents.mycli]
            type = "configurable"
            command = "mycli"
            submit_prompt = false
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.agents["mycli"].agent_type.as_deref(),
            Some("configurable")
        );
        assert_eq!(cfg.agents["mycli"].submit_prompt, Some(false));
    }
}
