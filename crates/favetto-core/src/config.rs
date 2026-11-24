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
//! tasks_dir = "~/.config/favetto/tasks"
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::mcp::McpTransport;
use crate::model::{Failure, FailureKind, PermissionPolicy};

#[derive(Debug, Clone, Default, JsonSchema, Serialize, Deserialize)]
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
    /// Repo-local task catalogs: the known-repos registry seed, the active repo,
    /// and the optional base directory scanned for repositories. Ignored by
    /// clients; the daemon persists changes in SQLite.
    #[serde(default)]
    pub repos: ReposSettings,
    /// MCP servers Favetto may forward to ACP agents. The daemon persists edits
    /// in SQLite; this section only seeds the registry at startup.
    #[serde(default)]
    pub mcp: McpSettings,
    /// Task executor concurrency / isolation.
    #[serde(default)]
    pub executor: ExecutorSettings,
    /// Non-interactive git provisioning for agent processes. Defaults to
    /// `signing = "off"`, which forces `commit.gpgsign = false` so an agent
    /// commit can never block on an interactive pinentry/askpass prompt.
    #[serde(default)]
    pub git: GitSettings,
    /// Client-side TUI settings. Ignored by the daemon, which reads the same file.
    #[serde(default)]
    pub tui: TuiSettings,
    /// Authentication settings for the username/password login policy.
    #[serde(default)]
    pub auth: AuthSettings,
}

/// Authentication settings: the username/password login policy.
///
/// Login attempts are rate-limited per `(username, client)`; `require_tls`
/// rejects non-loopback logins that did not arrive over HTTPS (directly or via
/// a proxy that sets `X-Forwarded-Proto: https`).
#[derive(Debug, Clone, JsonSchema, Serialize, Deserialize)]
pub struct AuthSettings {
    /// Failed `POST /auth/login` attempts allowed per `(username, client)` in
    /// `login_window_secs` before the client is locked out (0 disables the
    /// limit).
    #[serde(default = "default_login_rate_limit")]
    pub login_rate_limit: u32,
    /// Window, in seconds, over which `login_rate_limit` failures accumulate.
    #[serde(default = "default_login_window_secs")]
    pub login_window_secs: u64,
    /// Reject non-loopback logins that did not arrive over HTTPS.
    #[serde(default)]
    pub require_tls: bool,
}

impl Default for AuthSettings {
    fn default() -> Self {
        Self {
            login_rate_limit: default_login_rate_limit(),
            login_window_secs: default_login_window_secs(),
            require_tls: false,
        }
    }
}

/// TUI-only settings. The daemon parses but ignores this section.
#[derive(Debug, Clone, JsonSchema, Serialize, Deserialize)]
pub struct TuiSettings {
    #[serde(default)]
    pub sound: SoundSettings,
    /// Editor command for `e` on a Catalog task. Unset -> `$VISUAL`, then
    /// `$EDITOR`, then `vi`. May include arguments (e.g. `code --wait`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor: Option<String>,
    /// Built-in (`opencode`, `catppuccin`) or user theme name. A user theme is
    /// loaded from `themes/<name>.json` in the config or `.favetto/themes`
    /// directory. Overridable with `FAVETTO_THEME_NAME`.
    #[serde(default = "default_theme")]
    pub theme: String,
    /// Theme mode: `system` (derive from the terminal), `dark`, or `light`.
    /// Overridable with `FAVETTO_THEME`.
    #[serde(default = "default_theme_mode")]
    pub theme_mode: String,
    /// Show the tab bar. Default true.
    #[serde(default = "default_true")]
    pub show_header: bool,
    /// Show the status line. Default true.
    #[serde(default = "default_true")]
    pub show_footer: bool,
}

impl Default for TuiSettings {
    fn default() -> Self {
        Self {
            sound: SoundSettings::default(),
            editor: None,
            theme: default_theme(),
            theme_mode: default_theme_mode(),
            show_header: true,
            show_footer: true,
        }
    }
}

/// Client-side sound notifications for the TUI.
///
/// Precedence is CLI flags (`--sound`/`--no-sound`/`--sound-command`) over the
/// `FAVETTO_SOUND*` environment variables over this section over built-in
/// defaults. Sounds are played on the machine running `favetto tui`.
#[derive(Debug, Clone, JsonSchema, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Default, JsonSchema, Serialize, Deserialize)]
pub struct AgentSettings {
    /// Name of the default external agent (a key in `[agents.*]`). When set,
    /// catalog tasks run through this agent unless a task names its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// How an agent's live state is observed (see
/// `docs/design/agent-state-adapters.md`).
///
/// An external agent, launched as an Agent Client Protocol (ACP) server.
///
/// `command` and `args` must start the agent in its ACP mode (for example
/// `command = "opencode"`, `args = ["acp"]`). Favetto speaks ACP over the
/// process's stdin/stdout; prompts, tool calls and permission requests are
/// structured protocol messages, not terminal text. Adding another compatible
/// agent is configuration, not code.
#[derive(Debug, Clone, Default, JsonSchema, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Executable that starts the ACP server (looked up on `PATH`).
    pub command: String,
    /// Arguments that put the executable into its ACP server mode.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for the server process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Working directory for the server (defaults to the task's directory).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Default permission policy for this agent's sessions. A catalog task may
    /// override it with its own `permission` header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<PermissionPolicy>,
    /// Per-agent override of the global `[git]` section. Only the fields set
    /// here replace the global values; the rest are inherited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitSettings>,
}

/// How favetto provisions `git` in an agent process.
///
/// `off` is the safe default: agent commits are explicitly unsigned so they can
/// never wait for a passphrase prompt nobody can answer. `ssh`/`gpg` opt into
/// signed agent commits with a dedicated identity/key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, JsonSchema, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GitSigning {
    #[default]
    Off,
    Gpg,
    Ssh,
}

/// Non-interactive git provisioning for agent processes.
///
/// `signing = None` means "unset" so a per-agent/per-task override can be merged
/// over the global section; the effective default is [`GitSigning::Off`]. The
/// settings are injected into the agent's environment through git's *environment
/// config* (`GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_<i>`/`GIT_CONFIG_VALUE_<i>`, git ≥
/// 2.31) and `GIT_AUTHOR_*`/`GIT_COMMITTER_*`; the operator's real git config is
/// never touched.
#[derive(Debug, Clone, Default, PartialEq, JsonSchema, Serialize, Deserialize)]
pub struct GitSettings {
    /// `"off"` (default), `"ssh"`, or `"gpg"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing: Option<GitSigning>,
    /// Commit author/committer name (`user.name` + `GIT_AUTHOR_NAME`/`GIT_COMMITTER_NAME`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_name: Option<String>,
    /// Commit author/committer email (`user.email` + the matching git env vars).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_email: Option<String>,
    /// GPG key id/fingerprint, or SSH public-key path / literal key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_key: Option<String>,
    /// Env var (in the daemon's environment) holding the passphrase. The value is
    /// forwarded to the agent; prefer this for interactive-daemon setups.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase_env: Option<String>,
    /// Command (argv list) whose stdout is the passphrase. Embedded in the
    /// generated wrapper script, so the secret never lands in the config file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase_command: Option<Vec<String>>,
}

impl GitSettings {
    /// The effective signing mode, defaulting to [`GitSigning::Off`] when unset.
    pub fn effective_signing(&self) -> GitSigning {
        self.signing.unwrap_or(GitSigning::Off)
    }
}

#[derive(Debug, Clone, Default, JsonSchema, Serialize, Deserialize)]
pub struct DaemonSettings {
    pub listen: Option<String>,
    pub socket: Option<PathBuf>,
    pub tasks_dir: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
    /// Bounded database growth. On by default; `days = 0` keeps everything.
    #[serde(default)]
    pub retention: RetentionSettings,
}

/// Repo-local task catalog settings (`[repos]`).
///
/// `known` seeds the daemon's persisted known-repos registry at startup (the
/// registry survives config edits); `active` selects the daemon-wide active repo
/// by name at startup. `base_dir`, when set, is scanned for repositories so
/// cloned checkouts register themselves without a config edit. Repo task files
/// live in `<repo>/.favetto/tasks/`.
#[derive(Debug, Clone, JsonSchema, Serialize, Deserialize)]
pub struct ReposSettings {
    /// Repositories to register at startup, by daemon-host path.
    #[serde(default)]
    pub known: Vec<RepoSeed>,
    /// The active repo's name at startup; unset means the global catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    /// Directory whose repositories are discovered automatically. `~`-expanded;
    /// any child carrying a `.favetto/` directory is registered. Unset disables
    /// scanning. Discovered repos are reclaimed when their directory vanishes;
    /// `known`/`repos.add` entries are pinned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_dir: Option<PathBuf>,
    /// How many directory levels below `base_dir` to scan. `1` (the default)
    /// means immediate children; `2` also scans `org/repo` nesting.
    #[serde(default = "default_scan_depth")]
    pub depth: usize,
}

/// Default scan depth for [`ReposSettings::depth`]: immediate children only.
fn default_scan_depth() -> usize {
    1
}

impl Default for ReposSettings {
    fn default() -> Self {
        Self {
            known: Vec::new(),
            active: None,
            base_dir: None,
            depth: default_scan_depth(),
        }
    }
}

/// MCP server registry seed (`[mcp]`).
///
/// Every entry is operator-authored and therefore treated as pre-approved. The
/// daemon copies seeds into its persisted registry at startup; later app edits
/// survive restarts. Favetto never installs or upgrades the server.
#[derive(Debug, Clone, Default, JsonSchema, Serialize, Deserialize)]
pub struct McpSettings {
    #[serde(default)]
    pub servers: Vec<McpServerSeed>,
}

/// One `[[mcp.servers]]` entry.
///
/// `env`/`headers` hold literal secret values (stored by the daemon in its
/// `0600` secrets file, not in this config file); `env_from`/`headers_from` map
/// the entry name to the daemon env var carrying the value.
#[derive(Debug, Clone, JsonSchema, Serialize, Deserialize)]
pub struct McpServerSeed {
    /// Unique registry name, also used as the ACP server name.
    pub name: String,
    /// `stdio` (default), `http`, or `sse`.
    #[serde(default)]
    pub transport: McpTransport,
    /// Stdio executable; a bare name is resolved on `PATH` at seed time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// Literal environment values.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Environment entries read from a daemon env var (name -> source var).
    #[serde(default)]
    pub env_from: BTreeMap<String, String>,
    /// HTTP/SSE endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Literal HTTP header values.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// HTTP header entries read from a daemon env var (name -> source var).
    #[serde(default)]
    pub headers_from: BTreeMap<String, String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Operator-authored seeds are pre-approved; consent can still be revoked
    /// through `mcp.approve`.
    #[serde(default = "default_true")]
    pub approved: bool,
}

/// One `[[repos.known]]` entry: a repository path and an optional display name.
#[derive(Debug, Clone, JsonSchema, Serialize, Deserialize)]
pub struct RepoSeed {
    /// Absolute (or `~`-expanded) path to the repository on the daemon host.
    pub path: PathBuf,
    /// Display name used as the namespace key; defaults to the directory
    /// basename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

fn default_retention_days() -> u64 {
    30
}

fn default_retention_min_tasks() -> u64 {
    1000
}

fn default_max_output_bytes() -> usize {
    256 * 1024
}

/// Database retention policy.
///
/// This is the one setting that deletes data: on daemon start (and every six
/// hours) rows older than `days` are pruned and the database is optionally
/// `VACUUM`ed. `min_tasks` guarantees the newest N task rows survive regardless
/// of age. Set `days = 0` to opt out and keep everything forever.
///
/// Pruning happens in two stages. First an old task's `output` blob is cleared
/// (space reclaim with the metadata kept); per-run token/cost usage is stored in
/// flat `task_runs` columns, so it survives this stage and stays available to
/// `usage.stats`. Then, once the task row itself is old enough to delete, its
/// run rows (and their usage) go with it.
#[derive(Debug, Clone, JsonSchema, Serialize, Deserialize)]
pub struct RetentionSettings {
    /// Delete rows older than this many days (0 = keep forever).
    #[serde(default = "default_retention_days")]
    pub days: u64,
    /// Always keep at least this many newest task rows.
    #[serde(default = "default_retention_min_tasks")]
    pub min_tasks: u64,
    /// VACUUM + WAL checkpoint after a prune that deleted rows.
    #[serde(default = "default_true")]
    pub vacuum: bool,
}

impl Default for RetentionSettings {
    fn default() -> Self {
        Self {
            days: default_retention_days(),
            min_tasks: default_retention_min_tasks(),
            vacuum: true,
        }
    }
}

fn default_worktree_retention_days() -> u64 {
    30
}

/// Worktree retention policy. At daemon start (and every six hours) recorded
/// worktrees whose owning task is finished and older than `days` are removed,
/// along with their `favetto/*` branch; orphans whose task row was already
/// pruned are reclaimed regardless of age. `days = 0` keeps opt-in worktrees
/// forever. Active (`pending`/`running`/`awaiting_input`) tasks are never
/// touched.
#[derive(Debug, Clone, JsonSchema, Serialize, Deserialize)]
pub struct WorktreeRetentionSettings {
    /// Remove a finished task's worktree older than this many days (0 = keep forever).
    #[serde(default = "default_worktree_retention_days")]
    pub days: u64,
    /// Always keep at least this many newest finished worktrees.
    #[serde(default)]
    pub min_worktrees: u64,
}

impl Default for WorktreeRetentionSettings {
    fn default() -> Self {
        Self {
            days: default_worktree_retention_days(),
            min_worktrees: 0,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_theme() -> String {
    "opencode".to_string()
}

fn default_theme_mode() -> String {
    "system".to_string()
}

fn default_sound_player() -> String {
    "auto".to_string()
}

fn default_min_interval_ms() -> u64 {
    400
}

fn default_login_rate_limit() -> u32 {
    5
}

fn default_login_window_secs() -> u64 {
    300
}

fn default_sound_events() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("task_finished".to_string(), "success".to_string()),
        ("task_failed".to_string(), "failure".to_string()),
        ("task_started".to_string(), "none".to_string()),
        ("attention".to_string(), "none".to_string()),
        ("awaiting_input".to_string(), "attention".to_string()),
    ])
}

fn default_max_concurrency() -> usize {
    4
}

fn default_awaiting_input_quiet_ms() -> u64 {
    8000
}

fn default_retry_max_attempts() -> u32 {
    1
}

fn default_retry_initial_ms() -> u64 {
    5_000
}

fn default_retry_max_ms() -> u64 {
    300_000
}

fn default_retry_on() -> Vec<FailureKind> {
    vec![FailureKind::Infrastructure, FailureKind::Timeout]
}

/// What the startup reconciler does with a task whose in-flight run was left
/// behind by a previous daemon instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, JsonSchema, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StaleRunPolicy {
    /// Mark the owning task failed (preserves the pre-reconciler behaviour).
    #[default]
    Fail,
    /// Re-enqueue the owning task so the executor claims a fresh attempt.
    Retry,
}

/// How the delay between automatic retry attempts grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, JsonSchema, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backoff {
    /// `initial_ms * 2^(attempt - 1)`, capped at `max_ms`.
    #[default]
    Exponential,
    /// A constant `initial_ms` between every attempt.
    Fixed,
}

/// Automatic retry policy for retryable task failures (`[executor.retry]`).
///
/// Retries are opt-in: the default `max_attempts = 1` means a task runs exactly
/// once. When enabled, after a failed attempt whose [`FailureKind`] is listed in
/// `retry_on`, the executor re-enqueues the task for `attempt + 1` after the
/// backoff, recording one run per attempt. Agent and invalid-input failures are
/// excluded from the default `retry_on`, so a genuine failure is never retried
/// on its own.
#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct RetrySettings {
    /// Total execution attempts allowed per task, including the first. `1`
    /// (default) disables automatic retries.
    #[serde(default = "default_retry_max_attempts")]
    pub max_attempts: u32,
    /// How the delay between attempts grows.
    #[serde(default)]
    pub backoff: Backoff,
    /// Base delay before the second attempt, in milliseconds.
    #[serde(default = "default_retry_initial_ms")]
    pub initial_ms: u64,
    /// Upper bound on any single backoff delay, in milliseconds.
    #[serde(default = "default_retry_max_ms")]
    pub max_ms: u64,
    /// Failure kinds eligible for automatic retry. Defaults to infrastructure
    /// and timeout faults; `agent` and `invalid_input` should stay out.
    #[serde(default = "default_retry_on")]
    pub retry_on: Vec<FailureKind>,
}

impl Default for RetrySettings {
    fn default() -> Self {
        Self {
            max_attempts: default_retry_max_attempts(),
            backoff: Backoff::default(),
            initial_ms: default_retry_initial_ms(),
            max_ms: default_retry_max_ms(),
            retry_on: default_retry_on(),
        }
    }
}

impl RetrySettings {
    /// Whether `failure` from attempt `attempt` (1-based) should be retried
    /// automatically. The failure must be marked retryable (so a producer can
    /// veto a retry per failure), its kind must be enabled in `retry_on`, and
    /// the attempt budget (`max_attempts`) must not be exhausted.
    pub fn should_retry(&self, attempt: u32, failure: &Failure) -> bool {
        attempt >= 1
            && attempt < self.max_attempts
            && failure.retryable
            && self.retry_on.contains(&failure.kind)
    }

    /// The delay before the attempt that follows a failure at `attempt`
    /// (1-based). Exponential backoff doubles from `initial_ms`, capped at
    /// `max_ms`; fixed backoff always returns `initial_ms` (also capped).
    pub fn backoff_delay(&self, attempt: u32) -> Duration {
        let exponent = attempt.saturating_sub(1).min(31);
        let ms = match self.backoff {
            Backoff::Fixed => self.initial_ms,
            Backoff::Exponential => {
                let factor = 1u64.checked_shl(exponent).unwrap_or(u64::MAX);
                self.initial_ms.saturating_mul(factor)
            }
        };
        Duration::from_millis(ms.min(self.max_ms))
    }
}

/// How the executor runs tasks: concurrency and directory/worktree isolation.
///
/// With `parallel = true` each task gets a git worktree when its working
/// directory is inside a repository (so tasks can run side by side without
/// stepping on each other). Tasks that do **not** run in a worktree are
/// serialized per working directory.
#[derive(Debug, Clone, JsonSchema, Serialize, Deserialize)]
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
    /// Retention policy for worktrees left on disk.
    #[serde(default)]
    pub worktree_retention: WorktreeRetentionSettings,
    /// Keep worktrees after the task finishes (default false) so a completed
    /// task cleans up after itself. Set true to keep the agent's branch/changes
    /// for inspection; the retention sweep still reclaims kept worktrees after
    /// `worktree_retention.days`.
    #[serde(default)]
    pub keep_worktree: bool,
    /// Cap on the stored `task.output` blob, in bytes (default 256 KiB). Longer
    /// output is truncated head+tail and flagged.
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes: usize,
    /// Detect when a running agent is blocked waiting for user input and surface
    /// it as `awaiting_input` on the task (default true).
    #[serde(default = "default_true")]
    pub detect_awaiting_input: bool,
    /// How long the PTY must be quiet (ms) before the generic prompt detector
    /// fires (default 8000). Agent-specific detectors ignore this.
    #[serde(default = "default_awaiting_input_quiet_ms")]
    pub awaiting_input_quiet_ms: u64,
    /// Startup policy for a task left in flight by a previous daemon: `"fail"`
    /// (default) marks it failed, `"retry"` re-enqueues it for a fresh attempt.
    #[serde(default)]
    pub stale_run: StaleRunPolicy,
    /// Automatic retry policy for retryable failures. Disabled by default.
    #[serde(default)]
    pub retry: RetrySettings,
}

impl Default for ExecutorSettings {
    fn default() -> Self {
        Self {
            parallel: false,
            max_concurrency: default_max_concurrency(),
            worktree: true,
            worktree_dir: None,
            worktree_retention: WorktreeRetentionSettings::default(),
            keep_worktree: false,
            max_output_bytes: default_max_output_bytes(),
            detect_awaiting_input: true,
            awaiting_input_quiet_ms: default_awaiting_input_quiet_ms(),
            stale_run: StaleRunPolicy::default(),
            retry: RetrySettings::default(),
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

/// The placeholder written into a redacted config in place of a secret value.
///
/// `config.get` never returns literal webhook secrets, MCP `env`/`headers`
/// values or agent `env` values. A client echoes this sentinel back on save and
/// the daemon restores the value from the file on disk, so an unchanged secret
/// is never erased by an edit elsewhere.
pub const REDACTED_SECRET: &str = "__FAVETTO_REDACTED__";

impl FavettoConfig {
    /// Load from `path`, tolerating a missing file.
    ///
    /// The file is layered with `FAVETTO_*` environment overrides, matching the
    /// daemon's startup resolution. Draft validation uses [`Self::parse_str`]
    /// instead so the file's own values can be checked in isolation.
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

    /// Parse raw TOML into a config without applying environment overrides.
    ///
    /// This is the strict file-only path used by `config.validate` and
    /// `config.set`: a draft is accepted only when the file itself is valid.
    pub fn parse_str(raw: &str) -> anyhow::Result<Self> {
        Ok(toml::from_str(raw)?)
    }

    /// The revision identifier for a raw config: a SHA-256 of its bytes.
    ///
    /// A missing file is represented by the empty string, so its revision is
    /// stable and a save can still be guarded against a concurrent first write.
    pub fn revision(raw: &str) -> String {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(raw.as_bytes()))
    }

    /// Replace every secret value with [`REDACTED_SECRET`].
    ///
    /// Malformed TOML is redacted with a best-effort line scanner so a broken
    /// file can still be opened and repaired without exposing its secrets.
    pub fn redact(raw: &str) -> String {
        let Ok(mut doc) = raw.parse::<toml_edit::DocumentMut>() else {
            return redact_malformed(raw);
        };
        redact_document(&mut doc);
        doc.to_string()
    }

    /// Restore the [`REDACTED_SECRET`] placeholders in a draft from `existing`.
    ///
    /// A placeholder whose key is absent from the existing file is removed, so
    /// explicitly deleting a secret clears it. When no placeholder remains the
    /// draft is returned unchanged, preserving the client's formatting byte for
    /// byte.
    pub fn restore_redacted_secrets(draft: &str, existing: &str) -> anyhow::Result<String> {
        let mut doc = draft.parse::<toml_edit::DocumentMut>()?;
        let secrets = toml::from_str::<toml::Value>(existing)
            .map(|value| collect_secret_values(&value))
            .unwrap_or_default();
        let changed = restore_table(doc.as_table_mut(), "", &secrets);
        if changed {
            Ok(doc.to_string())
        } else {
            Ok(draft.to_string())
        }
    }

    /// This config as JSON with every secret value redacted.
    pub fn redacted_json(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        redact_json_secrets(&mut value);
        value
    }

    /// Write `contents` to `path` atomically, preserving an existing file mode.
    ///
    /// The file is written to a sibling temp file, fsynced, then renamed over the
    /// destination. A new file is created with mode `0600` because it can hold
    /// secrets.
    pub fn write_atomic(path: &Path, contents: &str) -> anyhow::Result<()> {
        use std::io::Write as _;
        use std::sync::atomic::{AtomicU64, Ordering};

        static COUNTER: AtomicU64 = AtomicU64::new(0);

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let permissions = std::fs::metadata(path)
            .ok()
            .map(|metadata| metadata.permissions());
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "config.toml".to_string());
        let temp = path.with_file_name(format!(
            ".{file_name}.tmp-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));

        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let write_result = (|| -> anyhow::Result<()> {
            let mut file = options.open(&temp)?;
            file.write_all(contents.as_bytes())?;
            file.sync_all()?;
            Ok(())
        })();
        if let Err(error) = write_result {
            let _ = std::fs::remove_file(&temp);
            return Err(error);
        }
        if let Some(permissions) = permissions {
            let _ = std::fs::set_permissions(&temp, permissions);
        }
        if let Err(error) = std::fs::rename(&temp, path) {
            let _ = std::fs::remove_file(&temp);
            return Err(error.into());
        }
        Ok(())
    }
}

/// Redact the secret paths of a parsed document in place.
fn redact_document(doc: &mut toml_edit::DocumentMut) {
    redact_table(doc.as_table_mut(), "");
}

/// The exact scalar secret paths.
fn is_secret_scalar(path: &str) -> bool {
    path == "webhook.github.secret"
}

/// Whether `path` names a map whose values are all secrets.
fn is_secret_map(path: &str) -> bool {
    (path == "agents" || path.starts_with("agents.")) && path.ends_with(".env")
        || path.starts_with("mcp.servers.")
            && (path.ends_with(".env") || path.ends_with(".headers"))
}

fn child_path(path: &str, key: impl std::fmt::Display) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

/// Redact a TOML table (the document root or an array-of-tables element).
fn redact_table(table: &mut toml_edit::Table, path: &str) {
    if is_secret_map(path) {
        for (_, value) in table.iter_mut() {
            redact_secret_item(value);
        }
        return;
    }
    for (key, item) in table.iter_mut() {
        redact_item(item, &child_path(path, key.get()));
    }
}

fn redact_item(item: &mut toml_edit::Item, path: &str) {
    if is_secret_scalar(path) {
        redact_secret_item(item);
        return;
    }
    if let Some(value) = item.as_value_mut() {
        redact_value(value, path);
        return;
    }
    if let Some(table) = item.as_table_mut() {
        redact_table(table, path);
        return;
    }
    if let Some(tables) = item.as_array_of_tables_mut() {
        for (index, table) in tables.iter_mut().enumerate() {
            redact_table(table, &child_path(path, index));
        }
    }
}

fn redact_value(value: &mut toml_edit::Value, path: &str) {
    if is_secret_map(path) {
        if let Some(table) = value.as_inline_table_mut() {
            for (_, value) in table.iter_mut() {
                *value = toml_edit::Value::from(REDACTED_SECRET);
            }
        }
        return;
    }
    if let Some(table) = value.as_inline_table_mut() {
        let keys: Vec<String> = table.iter().map(|(key, _)| key.to_string()).collect();
        for key in keys {
            let child_path = child_path(path, &key);
            if is_secret_scalar(&child_path) {
                if let Some(value) = table.get_mut(&key) {
                    *value = toml_edit::Value::from(REDACTED_SECRET);
                }
                continue;
            }
            if let Some(value) = table.get_mut(&key) {
                redact_value(value, &child_path);
            }
        }
        return;
    }
    if let Some(array) = value.as_array_mut() {
        for (index, value) in array.iter_mut().enumerate() {
            redact_value(value, &child_path(path, index));
        }
    }
}

/// Overwrite one scalar secret with the redaction sentinel.
fn redact_secret_item(item: &mut toml_edit::Item) {
    if item.as_value().is_some() {
        *item = toml_edit::Item::Value(toml_edit::Value::from(REDACTED_SECRET));
    }
}

/// Collect the scalar secret values of a config, keyed by their dotted path.
fn collect_secret_values(config: &toml::Value) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Some(secret) = config
        .get("webhook")
        .and_then(|value| value.get("github"))
        .and_then(|value| value.get("secret"))
        .and_then(toml::Value::as_str)
    {
        out.insert("webhook.github.secret".to_string(), secret.to_string());
    }
    if let Some(agents) = config.get("agents").and_then(toml::Value::as_table) {
        for (name, agent) in agents {
            if let Some(env) = agent.get("env").and_then(toml::Value::as_table) {
                for (key, value) in env {
                    if let Some(value) = value.as_str() {
                        out.insert(format!("agents.{name}.env.{key}"), value.to_string());
                    }
                }
            }
        }
    }
    if let Some(servers) = config
        .get("mcp")
        .and_then(|value| value.get("servers"))
        .and_then(toml::Value::as_array)
    {
        for (index, server) in servers.iter().enumerate() {
            for key in ["env", "headers"] {
                if let Some(map) = server.get(key).and_then(toml::Value::as_table) {
                    for (name, value) in map {
                        if let Some(value) = value.as_str() {
                            out.insert(
                                format!("mcp.servers.{index}.{key}.{name}"),
                                value.to_string(),
                            );
                        }
                    }
                }
            }
        }
    }
    out
}

/// Restore sentinels in a table (the document root or an array-of-tables
/// element), returning whether anything changed.
fn restore_table(
    table: &mut toml_edit::Table,
    path: &str,
    secrets: &BTreeMap<String, String>,
) -> bool {
    if is_secret_map(path) {
        return restore_map_table(table, path, secrets);
    }
    let mut changed = false;
    let keys: Vec<String> = table.iter().map(|(key, _)| key.to_string()).collect();
    for key in keys {
        let child_path = child_path(path, &key);
        if is_secret_scalar(&child_path) {
            let redacted = table.get(&key).is_some_and(is_redacted);
            if redacted {
                match secrets.get(&child_path) {
                    Some(value) => {
                        table.insert(
                            &key,
                            toml_edit::Item::Value(toml_edit::Value::from(value.clone())),
                        );
                    }
                    None => {
                        table.remove(&key);
                    }
                }
                changed = true;
            }
            continue;
        }
        if let Some(item) = table.get_mut(&key) {
            changed |= restore_item(item, &child_path, secrets);
        }
    }
    changed
}

fn restore_item(
    item: &mut toml_edit::Item,
    path: &str,
    secrets: &BTreeMap<String, String>,
) -> bool {
    if let Some(value) = item.as_value_mut() {
        return restore_value(value, path, secrets);
    }
    if let Some(table) = item.as_table_mut() {
        return restore_table(table, path, secrets);
    }
    if let Some(tables) = item.as_array_of_tables_mut() {
        let mut changed = false;
        for (index, table) in tables.iter_mut().enumerate() {
            changed |= restore_table(table, &child_path(path, index), secrets);
        }
        return changed;
    }
    false
}

fn restore_value(
    value: &mut toml_edit::Value,
    path: &str,
    secrets: &BTreeMap<String, String>,
) -> bool {
    if is_secret_map(path) {
        return restore_map_value(value, path, secrets);
    }
    if let Some(table) = value.as_inline_table_mut() {
        let mut changed = false;
        let keys: Vec<String> = table.iter().map(|(key, _)| key.to_string()).collect();
        for key in keys {
            let child_path = child_path(path, &key);
            if is_secret_scalar(&child_path) {
                let redacted = table
                    .get(&key)
                    .and_then(toml_edit::Value::as_str)
                    .is_some_and(|value| value == REDACTED_SECRET);
                if redacted {
                    match secrets.get(&child_path) {
                        Some(value) => {
                            table.insert(&key, toml_edit::Value::from(value.clone()));
                        }
                        None => {
                            table.remove(&key);
                        }
                    }
                    changed = true;
                }
                continue;
            }
            if let Some(value) = table.get_mut(&key) {
                changed |= restore_value(value, &child_path, secrets);
            }
        }
        return changed;
    }
    if let Some(array) = value.as_array_mut() {
        let mut changed = false;
        for (index, value) in array.iter_mut().enumerate() {
            changed |= restore_value(value, &child_path(path, index), secrets);
        }
        return changed;
    }
    false
}

fn restore_map_table(
    table: &mut toml_edit::Table,
    path: &str,
    secrets: &BTreeMap<String, String>,
) -> bool {
    let mut changed = false;
    let keys: Vec<String> = table.iter().map(|(key, _)| key.to_string()).collect();
    for key in keys {
        let redacted = table.get(&key).is_some_and(is_redacted);
        if !redacted {
            continue;
        }
        match secrets.get(&child_path(path, &key)) {
            Some(value) => {
                table.insert(
                    &key,
                    toml_edit::Item::Value(toml_edit::Value::from(value.clone())),
                );
            }
            None => {
                table.remove(&key);
            }
        }
        changed = true;
    }
    changed
}

fn restore_map_value(
    value: &mut toml_edit::Value,
    path: &str,
    secrets: &BTreeMap<String, String>,
) -> bool {
    let Some(table) = value.as_inline_table_mut() else {
        return false;
    };
    let mut changed = false;
    let keys: Vec<String> = table.iter().map(|(key, _)| key.to_string()).collect();
    for key in keys {
        let redacted = table
            .get(&key)
            .and_then(toml_edit::Value::as_str)
            .is_some_and(|value| value == REDACTED_SECRET);
        if !redacted {
            continue;
        }
        match secrets.get(&child_path(path, &key)) {
            Some(value) => {
                table.insert(&key, toml_edit::Value::from(value.clone()));
            }
            None => {
                table.remove(&key);
            }
        }
        changed = true;
    }
    changed
}

fn item_as_str(item: &toml_edit::Item) -> Option<&str> {
    item.as_value().and_then(toml_edit::Value::as_str)
}

fn is_redacted(item: &toml_edit::Item) -> bool {
    item_as_str(item).is_some_and(|value| value == REDACTED_SECRET)
}

/// Best-effort redaction for malformed TOML, where `toml_edit` cannot parse.
///
/// It blanks the value of every `secret` assignment and every assignment inside
/// an `env`/`headers` table so a broken file can still be shown for repair.
fn redact_malformed(raw: &str) -> String {
    let mut sensitive = false;
    let mut out = String::with_capacity(raw.len());
    for line in raw.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            sensitive = trimmed.contains(".env]")
                || trimmed.contains(".headers]")
                || trimmed.contains(".env.")
                || trimmed.contains(".headers.");
        }
        let Some((key, _)) = line.split_once('=') else {
            out.push_str(line);
            continue;
        };
        let key = key.trim();
        let is_secret = key == "secret"
            || key.ends_with(".secret")
            || (sensitive
                && !key.is_empty()
                && !key.contains(' ')
                && !key.contains('[')
                && !key.starts_with('#'));
        if !is_secret {
            out.push_str(line);
            continue;
        }
        let (prefix, _) = line.split_once('=').expect("checked split_once");
        let newline = if line.ends_with("\r\n") {
            "\r\n"
        } else if line.ends_with('\n') {
            "\n"
        } else {
            ""
        };
        out.push_str(prefix);
        out.push_str("= \"");
        out.push_str(REDACTED_SECRET);
        out.push('"');
        out.push_str(newline);
    }
    out
}

/// Redact the known secret paths of a JSON config value in place.
fn redact_json_secrets(value: &mut serde_json::Value) {
    if let Some(secret) = value.pointer_mut("/webhook/github/secret") {
        *secret = serde_json::Value::String(REDACTED_SECRET.to_string());
    }
    if let Some(agents) = value.get_mut("agents").and_then(|v| v.as_object_mut()) {
        for (_, agent) in agents.iter_mut() {
            redact_json_map(agent.get_mut("env"));
        }
    }
    if let Some(servers) = value
        .get_mut("mcp")
        .and_then(|v| v.get_mut("servers"))
        .and_then(serde_json::Value::as_array_mut)
    {
        for server in servers {
            redact_json_map(server.get_mut("env"));
            redact_json_map(server.get_mut("headers"));
        }
    }
}

fn redact_json_map(value: Option<&mut serde_json::Value>) {
    let Some(object) = value.and_then(serde_json::Value::as_object_mut) else {
        return;
    };
    for (_, value) in object.iter_mut() {
        *value = serde_json::Value::String(REDACTED_SECRET.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_legacy_webhook_section() {
        // Operators may still have `[webhook.github]` in their config file from
        // before the receivers were removed; the section is a silent no-op.
        let cfg: FavettoConfig = toml::from_str(
            r#"
            [webhook.github]
            enabled = true
            secret_env = "GITHUB_WEBHOOK_SECRET"

            [[webhook.github.rules]]
            name = "triage-opened-issues"
            event = "issues"
            task = "triage_favetto_issues"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.agents.len(), 0);
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
    fn parses_tui_editor() {
        let cfg: FavettoConfig = toml::from_str(
            r#"
            [tui]
            editor = "code --wait"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.tui.editor.as_deref(), Some("code --wait"));

        // Unset stays `None` so resolution can fall back to `$VISUAL`/`$EDITOR`.
        let default: FavettoConfig = toml::from_str("").unwrap();
        assert!(default.tui.editor.is_none());
    }

    #[test]
    fn tui_theme_defaults() {
        let cfg: FavettoConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.tui.theme, "opencode");
        assert_eq!(cfg.tui.theme_mode, "system");
        assert!(cfg.tui.show_header);
        assert!(cfg.tui.show_footer);
    }

    #[test]
    fn parses_tui_theme_keys() {
        let cfg: FavettoConfig = toml::from_str(
            r#"
            [tui]
            theme = "catppuccin"
            theme_mode = "light"
            show_header = false
            show_footer = false
            "#,
        )
        .unwrap();
        assert_eq!(cfg.tui.theme, "catppuccin");
        assert_eq!(cfg.tui.theme_mode, "light");
        assert!(!cfg.tui.show_header);
        assert!(!cfg.tui.show_footer);
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
    fn agent_config_parses_acp_fields() {
        let cfg: FavettoConfig = toml::from_str(
            r#"
            [agents.opencode]
            command = "opencode"
            args = ["acp"]

            [agents.custom]
            command = "mycli"
            args = ["--acp"]
            permission = "deny"
            "#,
        )
        .unwrap();

        let opencode = &cfg.agents["opencode"];
        assert_eq!(opencode.command, "opencode");
        assert_eq!(opencode.args, vec!["acp"]);
        assert!(opencode.permission.is_none());

        let custom = &cfg.agents["custom"];
        assert_eq!(custom.args, vec!["--acp"]);
        assert_eq!(custom.permission, Some(PermissionPolicy::Deny));

        // An absent policy inherits the agent entry default, then `ask`.
        let bare: FavettoConfig = toml::from_str("[agents.bare]\ncommand = \"x\"\n").unwrap();
        assert!(bare.agents["bare"].permission.is_none());
    }

    #[test]
    fn parses_git_settings() {
        let cfg: FavettoConfig = toml::from_str(
            r#"
            [git]
            signing = "ssh"
            user_name = "favetto agent"
            user_email = "agent@favetto.local"
            signing_key = "~/.config/favetto/agent_signing.pub"
            passphrase_env = "FAVETTO_GIT_SIGNING_PASSPHRASE"
            passphrase_command = ["secret-tool", "lookup", "service", "favetto"]
            "#,
        )
        .unwrap();

        let git = &cfg.git;
        assert_eq!(git.effective_signing(), GitSigning::Ssh);
        assert_eq!(git.user_name.as_deref(), Some("favetto agent"));
        assert_eq!(git.user_email.as_deref(), Some("agent@favetto.local"));
        assert_eq!(
            git.signing_key.as_deref(),
            Some("~/.config/favetto/agent_signing.pub")
        );
        assert_eq!(
            git.passphrase_env.as_deref(),
            Some("FAVETTO_GIT_SIGNING_PASSPHRASE")
        );
        assert_eq!(
            git.passphrase_command.as_deref(),
            Some(
                ["secret-tool", "lookup", "service", "favetto"]
                    .map(str::to_string)
                    .as_slice()
            )
        );
    }

    #[test]
    fn git_signing_defaults_off_when_absent() {
        let cfg: FavettoConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.git.effective_signing(), GitSigning::Off);
        assert!(cfg.git.signing.is_none());
        // An explicit `off` is also accepted.
        let off: FavettoConfig = toml::from_str("[git]\nsigning = \"off\"\n").unwrap();
        assert_eq!(off.git.effective_signing(), GitSigning::Off);
    }

    #[test]
    fn retention_and_output_cap_defaults() {
        let cfg: FavettoConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.daemon.retention.days, 30);
        assert_eq!(cfg.daemon.retention.min_tasks, 1000);
        assert!(cfg.daemon.retention.vacuum);
        assert_eq!(cfg.executor.max_output_bytes, 262_144);
        assert!(!cfg.executor.keep_worktree);
    }

    #[test]
    fn worktree_defaults_changed() {
        let cfg: FavettoConfig = toml::from_str("").unwrap();
        assert!(!cfg.executor.keep_worktree);
        assert_eq!(cfg.executor.worktree_retention.days, 30);
        assert_eq!(cfg.executor.worktree_retention.min_worktrees, 0);
    }

    #[test]
    fn stale_run_defaults_to_fail_and_parses_retry() {
        let cfg: FavettoConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.executor.stale_run, StaleRunPolicy::Fail);

        let retry: FavettoConfig = toml::from_str("[executor]\nstale_run = \"retry\"\n").unwrap();
        assert_eq!(retry.executor.stale_run, StaleRunPolicy::Retry);

        let explicit: FavettoConfig = toml::from_str("[executor]\nstale_run = \"fail\"\n").unwrap();
        assert_eq!(explicit.executor.stale_run, StaleRunPolicy::Fail);
    }

    #[test]
    fn retry_defaults_disable_automatic_retries() {
        let cfg: FavettoConfig = toml::from_str("").unwrap();
        let retry = &cfg.executor.retry;
        assert_eq!(retry.max_attempts, 1);
        assert_eq!(retry.backoff, Backoff::Exponential);
        assert_eq!(retry.initial_ms, 5_000);
        assert_eq!(retry.max_ms, 300_000);
        assert_eq!(
            retry.retry_on,
            vec![FailureKind::Infrastructure, FailureKind::Timeout]
        );
    }

    #[test]
    fn parses_executor_retry() {
        let cfg: FavettoConfig = toml::from_str(
            r#"
            [executor.retry]
            max_attempts = 4
            backoff = "fixed"
            initial_ms = 250
            max_ms = 1000
            retry_on = ["infrastructure", "agent"]
            "#,
        )
        .unwrap();
        let retry = &cfg.executor.retry;
        assert_eq!(retry.max_attempts, 4);
        assert_eq!(retry.backoff, Backoff::Fixed);
        assert_eq!(retry.initial_ms, 250);
        assert_eq!(retry.max_ms, 1000);
        assert_eq!(
            retry.retry_on,
            vec![FailureKind::Infrastructure, FailureKind::Agent]
        );
    }

    #[test]
    fn retry_decision_respects_kind_and_attempt_budget() {
        let retry = RetrySettings {
            max_attempts: 3,
            ..RetrySettings::default()
        };
        let infra = Failure::new(FailureKind::Infrastructure, "PTY died");
        let agent = Failure::new(FailureKind::Agent, "exit 1");

        assert!(retry.should_retry(1, &infra));
        assert!(retry.should_retry(2, &infra));
        // The third attempt is the last: no fourth attempt.
        assert!(!retry.should_retry(3, &infra));
        // Attempt 0 means the task never ran: nothing to retry.
        assert!(!retry.should_retry(0, &infra));
        // Agent failures are never in the default `retry_on`.
        assert!(!retry.should_retry(1, &agent));
        // A producer can veto a retry even for an enabled kind.
        let vetoed = Failure {
            kind: FailureKind::Infrastructure,
            message: "do not retry".to_string(),
            retryable: false,
        };
        assert!(!retry.should_retry(1, &vetoed));
        // Disabled by default.
        assert!(!RetrySettings::default().should_retry(1, &infra));
    }

    #[test]
    fn backoff_delay_grows_exponentially_and_is_capped() {
        let retry = RetrySettings {
            backoff: Backoff::Exponential,
            initial_ms: 100,
            max_ms: 1_000,
            ..RetrySettings::default()
        };
        assert_eq!(retry.backoff_delay(1), Duration::from_millis(100));
        assert_eq!(retry.backoff_delay(2), Duration::from_millis(200));
        assert_eq!(retry.backoff_delay(3), Duration::from_millis(400));
        // 100 * 2^4 = 1600, capped at max_ms.
        assert_eq!(retry.backoff_delay(5), Duration::from_millis(1_000));

        let fixed = RetrySettings {
            backoff: Backoff::Fixed,
            initial_ms: 100,
            max_ms: 1_000,
            ..RetrySettings::default()
        };
        assert_eq!(fixed.backoff_delay(1), Duration::from_millis(100));
        assert_eq!(fixed.backoff_delay(9), Duration::from_millis(100));
    }

    #[test]
    fn parses_worktree_retention() {
        let cfg: FavettoConfig = toml::from_str(
            r#"
            [executor]
            keep_worktree = true

            [executor.worktree_retention]
            days = 2
            min_worktrees = 5
            "#,
        )
        .unwrap();
        assert!(cfg.executor.keep_worktree);
        assert_eq!(cfg.executor.worktree_retention.days, 2);
        assert_eq!(cfg.executor.worktree_retention.min_worktrees, 5);
    }

    #[test]
    fn parses_retention_and_output_cap() {
        let cfg: FavettoConfig = toml::from_str(
            r#"
            [daemon.retention]
            days = 7
            min_tasks = 50
            vacuum = false

            [executor]
            max_output_bytes = 4096
            "#,
        )
        .unwrap();
        assert_eq!(cfg.daemon.retention.days, 7);
        assert_eq!(cfg.daemon.retention.min_tasks, 50);
        assert!(!cfg.daemon.retention.vacuum);
        assert_eq!(cfg.executor.max_output_bytes, 4096);
    }

    #[test]
    fn retention_days_zero_disables_pruning() {
        let cfg: FavettoConfig = toml::from_str("[daemon.retention]\ndays = 0\n").unwrap();
        assert_eq!(cfg.daemon.retention.days, 0);
        // Other retention defaults still apply.
        assert_eq!(cfg.daemon.retention.min_tasks, 1000);
        assert!(cfg.daemon.retention.vacuum);
    }

    #[test]
    fn load_from_keeps_mcp_seed_env_values() {
        let path = std::env::temp_dir().join(format!(
            "favetto-mcp-config-{}-{}.toml",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::write(
            &path,
            "[[mcp.servers]]\nname = \"seeded\"\ncommand = \"/bin/sh\"\nenv = { SEED_TOKEN = \"seed-secret\" }\n",
        )
        .unwrap();
        let cfg = FavettoConfig::load_from(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            cfg.mcp.servers[0].env.get("SEED_TOKEN").map(String::as_str),
            Some("seed-secret")
        );
    }

    #[test]
    fn parses_mcp_server_seeds() {
        let cfg: FavettoConfig = toml::from_str(
            r#"
            [[mcp.servers]]
            name = "seeded"
            command = "/bin/sh"
            args = ["-c", "true"]
            env = { SEED_TOKEN = "seed-secret" }

            [[mcp.servers]]
            name = "web"
            transport = "http"
            url = "https://example.com/mcp"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.mcp.servers.len(), 2);
        let seeded = &cfg.mcp.servers[0];
        assert_eq!(
            seeded.env.get("SEED_TOKEN").map(String::as_str),
            Some("seed-secret")
        );
        assert!(seeded.enabled);
        assert!(seeded.approved);
        assert_eq!(cfg.mcp.servers[1].transport, McpTransport::Http);
    }

    #[test]
    fn parses_per_agent_git_override() {
        let cfg: FavettoConfig = toml::from_str(
            r#"
            [git]
            signing = "off"

            [agents.opencode]
            command = "opencode"

            [agents.opencode.git]
            signing = "ssh"
            signing_key = "~/.ssh/agent_ed25519.pub"
            "#,
        )
        .unwrap();

        let agent = &cfg.agents["opencode"];
        let git = agent.git.as_ref().expect("agent override parsed");
        assert_eq!(git.effective_signing(), GitSigning::Ssh);
        assert_eq!(git.signing_key.as_deref(), Some("~/.ssh/agent_ed25519.pub"));
        // The global section is untouched by the per-agent override.
        assert_eq!(cfg.git.effective_signing(), GitSigning::Off);
    }

    #[test]
    fn awaiting_input_defaults_and_overrides() {
        let cfg: FavettoConfig = toml::from_str("").unwrap();
        assert!(cfg.executor.detect_awaiting_input);
        assert_eq!(cfg.executor.awaiting_input_quiet_ms, 8000);

        let overridden: FavettoConfig = toml::from_str(
            r#"
            [executor]
            detect_awaiting_input = false
            awaiting_input_quiet_ms = 250
            "#,
        )
        .unwrap();
        assert!(!overridden.executor.detect_awaiting_input);
        assert_eq!(overridden.executor.awaiting_input_quiet_ms, 250);
    }

    #[test]
    fn auth_login_defaults_and_parses() {
        let cfg: FavettoConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.auth.login_rate_limit, 5);
        assert_eq!(cfg.auth.login_window_secs, 300);
        assert!(!cfg.auth.require_tls);

        let overridden: FavettoConfig = toml::from_str(
            r#"
            [auth]
            login_rate_limit = 2
            login_window_secs = 60
            require_tls = true
            "#,
        )
        .unwrap();
        assert_eq!(overridden.auth.login_rate_limit, 2);
        assert_eq!(overridden.auth.login_window_secs, 60);
        assert!(overridden.auth.require_tls);
    }

    #[test]
    fn default_sound_events_includes_awaiting_input() {
        let cfg = SoundSettings::default();
        assert_eq!(
            cfg.events.get("awaiting_input").map(String::as_str),
            Some("attention")
        );
    }

    #[test]
    fn auth_defaults_apply_to_a_legacy_config() {
        // A config written before `[auth]` existed must still decode, with the
        // documented defaults filled in.
        let cfg: FavettoConfig = toml::from_str(
            r#"
            [daemon]
            listen = "127.0.0.1:7878"

            [tui.sound]
            enabled = false
            "#,
        )
        .unwrap();
        assert_eq!(cfg.auth.login_rate_limit, 5);
        assert_eq!(cfg.auth.login_window_secs, 300);
        assert!(!cfg.auth.require_tls);
    }

    #[test]
    fn parses_auth_overrides() {
        let cfg: FavettoConfig = toml::from_str(
            r#"
            [auth]
            login_rate_limit = 10
            "#,
        )
        .unwrap();
        assert_eq!(cfg.auth.login_rate_limit, 10);
    }

    #[test]
    fn parses_repos_settings() {
        let cfg: FavettoConfig = toml::from_str(
            r#"
            [repos]
            active = "favetto"
            base_dir = "~/code"
            depth = 2

            [[repos.known]]
            path = "~/code/favetto"

            [[repos.known]]
            path = "/srv/web"
            name = "web"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.repos.active.as_deref(), Some("favetto"));
        assert_eq!(cfg.repos.known.len(), 2);
        assert_eq!(cfg.repos.known[0].path, PathBuf::from("~/code/favetto"));
        assert!(cfg.repos.known[0].name.is_none());
        assert_eq!(cfg.repos.known[1].path, PathBuf::from("/srv/web"));
        assert_eq!(cfg.repos.known[1].name.as_deref(), Some("web"));
        assert_eq!(cfg.repos.base_dir.as_deref(), Some(Path::new("~/code")));
        assert_eq!(cfg.repos.depth, 2);

        // Absent -> empty registry, no active repo, no scan root, depth 1.
        let default: FavettoConfig = toml::from_str("").unwrap();
        assert!(default.repos.known.is_empty());
        assert!(default.repos.active.is_none());
        assert!(default.repos.base_dir.is_none());
        assert_eq!(default.repos.depth, 1);
    }

    /// A unique temp path that does not exist on disk.
    fn unique_config_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "favetto-config-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn load_from_missing_path_is_valid_and_yields_defaults() {
        let path = unique_config_path("missing").join("config.toml");
        assert!(!path.exists());
        let cfg = FavettoConfig::load_from(&path).unwrap();
        assert_eq!(
            serde_json::to_value(&cfg).unwrap(),
            serde_json::to_value(FavettoConfig::default()).unwrap()
        );
    }

    #[test]
    fn load_from_malformed_file_is_an_error() {
        let dir = unique_config_path("malformed");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "not valid toml [[[").unwrap();
        let err = FavettoConfig::load_from(&path).unwrap_err();
        assert!(!err.to_string().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_str_is_strict_and_ignores_the_environment() {
        let cfg = FavettoConfig::parse_str("").unwrap();
        assert_eq!(cfg.daemon.listen, None);
        assert!(FavettoConfig::parse_str("[daemon]\nlisten = 42\n").is_err());
    }

    #[test]
    fn revision_is_stable_and_changes_with_the_bytes() {
        let first = FavettoConfig::revision("[daemon]\nlisten = \"a\"\n");
        assert_eq!(first, FavettoConfig::revision("[daemon]\nlisten = \"a\"\n"));
        assert_ne!(first, FavettoConfig::revision("[daemon]\nlisten = \"b\"\n"));
        assert_eq!(first.len(), 64);
        assert_eq!(FavettoConfig::revision(""), FavettoConfig::revision(""));
    }

    #[test]
    fn redact_blanks_every_secret_key() {
        let raw = r#"# top comment
[webhook.github]
enabled = true
secret = "wh"

[agents.custom]
command = "x"

[agents.custom.env]
A = "agent-secret"
B = "second"

[[mcp.servers]]
name = "s"
command = "x"
headers = { Authorization = "bearer secret" }

[mcp.servers.env]
C = "mcp-secret"
"#;
        let redacted = FavettoConfig::redact(raw);
        for secret in [
            "wh",
            "agent-secret",
            "second",
            "bearer secret",
            "mcp-secret",
        ] {
            assert!(!redacted.contains(secret), "`{secret}` leaked:\n{redacted}");
        }
        assert!(redacted.contains("# top comment"));
        assert!(redacted.contains(REDACTED_SECRET));

        // Malformed TOML is still redacted by the fallback scanner.
        let broken =
            "[webhook.github]\nsecret = \"boom\"\n[agents.x.env]\nTOKEN = \"nope\"\nnot toml [[[";
        let redacted = FavettoConfig::redact(broken);
        assert!(!redacted.contains("boom"), "{redacted}");
        assert!(!redacted.contains("nope"), "{redacted}");
    }

    #[test]
    fn restore_redacted_secrets_round_trips_and_honours_removal() {
        let existing = r#"[webhook.github]
secret = "wh"

[agents.custom]
command = "x"
[agents.custom.env]
A = "agent-secret"

[[mcp.servers]]
name = "s"
command = "x"
[mcp.servers.env]
C = "mcp-secret"
"#;
        let draft = FavettoConfig::redact(existing);
        let restored = FavettoConfig::restore_redacted_secrets(&draft, existing).unwrap();
        let cfg = FavettoConfig::parse_str(&restored).unwrap();
        assert!(restored.contains("secret = \"wh\""), "{restored}");
        assert_eq!(
            cfg.agents["custom"].env.get("A").map(String::as_str),
            Some("agent-secret")
        );
        assert_eq!(
            cfg.mcp.servers[0].env.get("C").map(String::as_str),
            Some("mcp-secret")
        );

        // An explicit deletion (the key is gone from the draft) clears it.
        let mut document: toml_edit::DocumentMut = draft.parse().unwrap();
        document["agents"]["custom"]["env"]
            .as_table_mut()
            .unwrap()
            .remove("A");
        let restored =
            FavettoConfig::restore_redacted_secrets(&document.to_string(), existing).unwrap();
        let cfg = FavettoConfig::parse_str(&restored).unwrap();
        assert!(!cfg.agents["custom"].env.contains_key("A"));
    }

    #[test]
    fn redacts_and_restores_inline_tables_too() {
        let existing = r#"webhook = { github = { secret = "wh" } }
agents = { custom = { command = "x", env = { A = "agent-secret" } } }
mcp = { servers = [{ name = "s", command = "x", env = { C = "mcp-secret" } }] }
"#;
        let redacted = FavettoConfig::redact(existing);
        assert!(!redacted.contains("agent-secret"), "{redacted}");
        assert!(!redacted.contains("mcp-secret"), "{redacted}");

        let restored = FavettoConfig::restore_redacted_secrets(&redacted, existing).unwrap();
        let cfg = FavettoConfig::parse_str(&restored).unwrap();
        assert!(restored.contains("secret = \"wh\""), "{restored}");
        assert_eq!(
            cfg.agents["custom"].env.get("A").map(String::as_str),
            Some("agent-secret")
        );
        assert_eq!(
            cfg.mcp.servers[0].env.get("C").map(String::as_str),
            Some("mcp-secret")
        );
    }

    #[test]
    fn restore_without_sentinels_preserves_the_draft_byte_for_byte() {
        let draft = "# comment\n[daemon]\nlisten = \"127.0.0.1:7878\"\n";
        let restored = FavettoConfig::restore_redacted_secrets(
            draft,
            "[daemon]\nlisten = \"127.0.0.1:9999\"\n",
        )
        .unwrap();
        assert_eq!(restored, draft);
    }

    #[test]
    fn write_atomic_preserves_mode_and_replaces_the_file() {
        let dir = unique_config_path("atomic");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "old").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        }
        FavettoConfig::write_atomic(&path, "new contents\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new contents\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o640);
        }
        // No temp sibling is left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name() != "config.toml")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn redacted_json_never_contains_secret_values() {
        let cfg = FavettoConfig::parse_str(
            r#"
            [webhook.github]
            secret = "wh"

            [agents.custom]
            command = "x"
            [agents.custom.env]
            A = "agent-secret"

            [[mcp.servers]]
            name = "s"
            command = "x"
            [mcp.servers.env]
            C = "mcp-secret"
            "#,
        )
        .unwrap();
        let json = cfg.redacted_json().to_string();
        assert!(!json.contains("agent-secret"), "{json}");
        assert!(!json.contains("mcp-secret"), "{json}");
        assert!(json.contains(REDACTED_SECRET), "{json}");
    }
}
