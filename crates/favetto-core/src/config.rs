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
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::model::{Failure, FailureKind};

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
    /// Task executor concurrency / isolation.
    #[serde(default)]
    pub executor: ExecutorSettings,
    /// Non-interactive git provisioning for agent processes. Defaults to
    /// `signing = "off"`, which forces `commit.gpgsign = false` so an agent
    /// commit can never block on an interactive pinentry/askpass prompt.
    #[serde(default)]
    pub git: GitSettings,
    /// Webhook trigger rules (currently GitHub).
    #[serde(default)]
    pub webhook: WebhookSettings,
    /// Client-side TUI settings. Ignored by the daemon, which reads the same file.
    #[serde(default)]
    pub tui: TuiSettings,
    /// Web client (embedded SPA) settings.
    #[serde(default)]
    pub web: WebSettings,
    /// Short-lived authentication ticket settings.
    #[serde(default)]
    pub auth: AuthSettings,
}

/// Web client (embedded SPA) settings.
///
/// `dir` overrides the assets embedded in the binary with a directory on disk
/// (development); leave it empty to serve the embedded assets. `heartbeat_secs`
/// is the interval of the SSE keep-alive comments that defeat idle proxy
/// timeouts.
#[derive(Debug, Clone, JsonSchema, Serialize, Deserialize)]
pub struct WebSettings {
    /// Serve the embedded web client from the daemon.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Asset directory override; empty serves the embedded assets.
    #[serde(default)]
    pub dir: PathBuf,
    /// SSE keep-alive interval, in seconds.
    #[serde(default = "default_web_heartbeat_secs")]
    pub heartbeat_secs: u64,
}

impl Default for WebSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: PathBuf::new(),
            heartbeat_secs: default_web_heartbeat_secs(),
        }
    }
}

/// Short-lived, single-use tickets for header-less clients (browser
/// WebSocket/SSE auth). The ticket is minted over authenticated HTTP and then
/// presented in a query string, where an `Authorization` header cannot go.
#[derive(Debug, Clone, JsonSchema, Serialize, Deserialize)]
pub struct AuthSettings {
    /// Ticket lifetime, in seconds.
    #[serde(default = "default_ticket_ttl_secs")]
    pub ticket_ttl_secs: u64,
}

impl Default for AuthSettings {
    fn default() -> Self {
        Self {
            ticket_ttl_secs: default_ticket_ttl_secs(),
        }
    }
}

/// TUI-only settings. The daemon parses but ignores this section.
#[derive(Debug, Clone, Default, JsonSchema, Serialize, Deserialize)]
pub struct TuiSettings {
    #[serde(default)]
    pub sound: SoundSettings,
    /// Editor command for `e` on a Catalog task. Unset -> `$VISUAL`, then
    /// `$EDITOR`, then `vi`. May include arguments (e.g. `code --wait`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor: Option<String>,
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

/// Webhook trigger settings. Currently only GitHub is supported.
#[derive(Debug, Clone, Default, JsonSchema, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Default, JsonSchema, Serialize, Deserialize)]
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
#[derive(Debug, Clone, JsonSchema, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Default, JsonSchema, Serialize, Deserialize)]
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
/// `auto` keeps the built-in default for the agent; `none` disables structured
/// observation entirely (the debounced screen heuristic still runs); `stdout`
/// parses the CLI's line-delimited JSON output; `server` observes a long-lived
/// HTTP/SSE endpoint (opencode); `hooks` observes via per-launch HTTP hooks
/// (claude). A custom agent has no built-in transport, so anything but `auto`
/// is purely declarative for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStateMode {
    /// Use the built-in default for this agent.
    Auto,
    /// Disable structured observation (screen fallback only).
    None,
    /// Observe the CLI's line-delimited JSON output.
    Stdout,
    /// Observe a long-lived HTTP/SSE endpoint.
    Server,
    /// Observe via per-launch HTTP hooks.
    Hooks,
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
#[derive(Debug, Clone, Default, JsonSchema, Serialize, Deserialize)]
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
    /// How to observe the agent's live state. Unset (or `auto`) keeps the
    /// built-in default for the agent; `none` forces the screen fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<AgentStateMode>,
    /// Structured stdout format for headless runs and final summaries, e.g.
    /// `"opencode-json"`, `"claude-stream-json"`, `"pi-json"`, `"pi-rpc"`,
    /// `"vibe-streaming"`, or `"plain-jsonl"`. Unset keeps the default
    /// `{ "text": raw }` output. A custom (`configurable`) agent only has a
    /// parser for `"plain-jsonl"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_format: Option<String>,
    /// opencode only: which server to observe. `"managed"` (favetto-owned
    /// `serve`), `"background"` (the registered background service), or a URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// claude only: inject favetto's HTTP hooks through a per-launch
    /// `--settings` file. Unset keeps the built-in default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<bool>,
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

fn default_sound_player() -> String {
    "auto".to_string()
}

fn default_min_interval_ms() -> u64 {
    400
}

fn default_web_heartbeat_secs() -> u64 {
    15
}

fn default_ticket_ttl_secs() -> u64 {
    30
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
    fn agent_config_state_and_output_format_parse() {
        let cfg: FavettoConfig = toml::from_str(
            r#"
            [agents.custom]
            command = "mycli"
            state = "stdout"
            output_format = "plain-jsonl"
            server = "managed"

            [agents.claude]
            command = "claude"
            state = "hooks"
            hooks = true
            "#,
        )
        .unwrap();

        let custom = &cfg.agents["custom"];
        assert_eq!(custom.state, Some(AgentStateMode::Stdout));
        assert_eq!(custom.output_format.as_deref(), Some("plain-jsonl"));
        assert_eq!(custom.server.as_deref(), Some("managed"));
        assert!(custom.hooks.is_none());

        let claude = &cfg.agents["claude"];
        assert_eq!(claude.state, Some(AgentStateMode::Hooks));
        assert_eq!(claude.hooks, Some(true));

        // Unknown/absent fields stay unset.
        let empty: FavettoConfig = toml::from_str("[agents.bare]\ncommand = \"x\"\n").unwrap();
        assert!(empty.agents["bare"].state.is_none());
        assert!(empty.agents["bare"].output_format.is_none());
        assert!(empty.agents["bare"].server.is_none());
        assert!(empty.agents["bare"].hooks.is_none());
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
    fn auth_ticket_ttl_defaults_and_parses() {
        let cfg: FavettoConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.auth.ticket_ttl_secs, 30);

        let overridden: FavettoConfig = toml::from_str("[auth]\nticket_ttl_secs = 5\n").unwrap();
        assert_eq!(overridden.auth.ticket_ttl_secs, 5);
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
    fn web_and_auth_defaults_apply_to_a_legacy_config() {
        // A config written before `[web]`/`[auth]` existed must still decode,
        // with the documented defaults filled in.
        let cfg: FavettoConfig = toml::from_str(
            r#"
            [daemon]
            listen = "127.0.0.1:7878"

            [tui.sound]
            enabled = false
            "#,
        )
        .unwrap();
        assert!(cfg.web.enabled);
        assert_eq!(cfg.web.dir, PathBuf::new());
        assert_eq!(cfg.web.heartbeat_secs, 15);
        assert_eq!(cfg.auth.ticket_ttl_secs, 30);
    }

    #[test]
    fn parses_web_and_auth_overrides() {
        let cfg: FavettoConfig = toml::from_str(
            r#"
            [web]
            enabled = false
            dir = "/srv/favetto/web"
            heartbeat_secs = 5

            [auth]
            ticket_ttl_secs = 60
            "#,
        )
        .unwrap();
        assert!(!cfg.web.enabled);
        assert_eq!(cfg.web.dir, PathBuf::from("/srv/favetto/web"));
        assert_eq!(cfg.web.heartbeat_secs, 5);
        assert_eq!(cfg.auth.ticket_ttl_secs, 60);
    }
}
