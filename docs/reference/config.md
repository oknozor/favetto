# Configuration reference

Generated from `crates/favetto-core/src/config.rs` by the docs generator. Do not edit by hand.

Every section is optional; favetto falls back to built-in defaults for anything you omit. Values resolve in the order **CLI flag → config file → built-in default**, and every `FAVETTO__SECTION__KEY` environment variable overrides the file.

## FavettoConfig

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `agent` | AgentSettings | `{}` | no | Default external agent used for catalog tasks and new agent sessions. |
| `agents` | Map<String, AgentConfig> | `{}` | no | External coding agents by name (e.g. `claude`, `opencode`, `pi`, `vibe`). |
| `daemon` | DaemonSettings | `{"listen":null,"socket":null,"tasks_dir":null,"data_dir":null,"retention":{"days":30,"min_tasks":1000,"vacuum":true}}` | no | Daemon defaults (overridable by CLI flags). |
| `repos` | ReposSettings | `{"known":[],"depth":1}` | no | Repo-local task catalogs: the known-repos registry seed, the active repo, and the optional base directory scanned for repositories. Ignored by clients; the daemon persists changes in SQLite. |
| `mcp` | McpSettings | `{"servers":[]}` | no | MCP servers Favetto may forward to ACP agents. The daemon persists edits in SQLite; this section only seeds the registry at startup. |
| `executor` | ExecutorSettings | `{"parallel":false,"max_concurrency":4,"worktree":true,"worktree_retention":{"days":30,"min_worktrees":0},"keep_worktree":false,"max_output_bytes":262144,"detect_awaiting_input":true,"awaiting_input_quiet_ms":8000,"stale_run":"fail","retry":{"max_attempts":1,"backoff":"exponential","initial_ms":5000,"max_ms":300000,"retry_on":["infrastructure","timeout"]}}` | no | Task executor concurrency / isolation. |
| `git` | GitSettings | `{}` | no | Non-interactive git provisioning for agent processes. Defaults to `signing = "off"`, which forces `commit.gpgsign = false` so an agent commit can never block on an interactive pinentry/askpass prompt. |
| `tui` | TuiSettings | `{"sound":{"enabled":true,"player":"auto","min_interval_ms":400,"only_when_unfocused":false,"events":{"attention":"none","awaiting_input":"attention","task_failed":"failure","task_finished":"success","task_started":"none"}},"theme":"opencode","theme_mode":"system","show_header":true,"show_footer":true}` | no | Client-side TUI settings. Ignored by the daemon, which reads the same file. |
| `auth` | AuthSettings | `{"login_rate_limit":5,"login_window_secs":300,"require_tls":false}` | no | Authentication settings for the username/password login policy. |

## AgentSettings

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `default` | string (optional) | — | no | Name of the default external agent (a key in `[agents.*]`). When set, catalog tasks run through this agent unless a task names its own. |

## AgentConfig

How an agent's live state is observed (see
`docs/design/agent-state-adapters.md`).

An external agent, launched as an Agent Client Protocol (ACP) server.

`command` and `args` must start the agent in its ACP mode (for example
`command = "opencode"`, `args = ["acp"]`). Favetto speaks ACP over the
process's stdin/stdout; prompts, tool calls and permission requests are
structured protocol messages, not terminal text. Adding another compatible
agent is configuration, not code.

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `command` | string | — | yes | Executable that starts the ACP server (looked up on `PATH`). |
| `args` | string[] | `[]` | no | Arguments that put the executable into its ACP server mode. |
| `env` | Map<String, string> | `{}` | no | Extra environment variables for the server process. |
| `cwd` | string (optional) | — | no | Working directory for the server (defaults to the task's directory). |
| `permission` | PermissionPolicy (optional) | — | no | Default permission policy for this agent's sessions. A catalog task may override it with its own `permission` header. |
| `git` | GitSettings (optional) | — | no | Per-agent override of the global `[git]` section. Only the fields set here replace the global values; the rest are inherited. |

## PermissionPolicy

How a session answers tool-permission requests raised by its agent.

The policy belongs to the task definition (or the agent entry as a default),
so an unattended run can choose `deny` while an attended run asks the user.
Agents that never ask (because their own configuration auto-approves) are
unaffected: the policy only decides how a request that *does* arrive is
answered.

string

## GitSettings

Non-interactive git provisioning for agent processes.

`signing = None` means "unset" so a per-agent/per-task override can be merged
over the global section; the effective default is [`GitSigning::Off`]. The
settings are injected into the agent's environment through git's *environment
config* (`GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_<i>`/`GIT_CONFIG_VALUE_<i>`, git ≥
2.31) and `GIT_AUTHOR_*`/`GIT_COMMITTER_*`; the operator's real git config is
never touched.

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `signing` | GitSigning (optional) | — | no | `"off"` (default), `"ssh"`, or `"gpg"`. |
| `user_name` | string (optional) | — | no | Commit author/committer name (`user.name` + `GIT_AUTHOR_NAME`/`GIT_COMMITTER_NAME`). |
| `user_email` | string (optional) | — | no | Commit author/committer email (`user.email` + the matching git env vars). |
| `signing_key` | string (optional) | — | no | GPG key id/fingerprint, or SSH public-key path / literal key. |
| `passphrase_env` | string (optional) | — | no | Env var (in the daemon's environment) holding the passphrase. The value is forwarded to the agent; prefer this for interactive-daemon setups. |
| `passphrase_command` | string[] (optional) | — | no | Command (argv list) whose stdout is the passphrase. Embedded in the generated wrapper script, so the secret never lands in the config file. |

## GitSigning

How favetto provisions `git` in an agent process.

`off` is the safe default: agent commits are explicitly unsigned so they can
never wait for a passphrase prompt nobody can answer. `ssh`/`gpg` opt into
signed agent commits with a dedicated identity/key.

`off` | `gpg` | `ssh`

## DaemonSettings

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `listen` | string (optional) | — | no |  |
| `socket` | string (optional) | — | no |  |
| `tasks_dir` | string (optional) | — | no |  |
| `data_dir` | string (optional) | — | no |  |
| `retention` | RetentionSettings | `{"days":30,"min_tasks":1000,"vacuum":true}` | no | Bounded database growth. On by default; `days = 0` keeps everything. |

## RetentionSettings

Database retention policy.

This is the one setting that deletes data: on daemon start (and every six
hours) rows older than `days` are pruned and the database is optionally
`VACUUM`ed. `min_tasks` guarantees the newest N task rows survive regardless
of age. Set `days = 0` to opt out and keep everything forever.

Pruning happens in two stages. First an old task's `output` blob is cleared
(space reclaim with the metadata kept); per-run token/cost usage is stored in
flat `task_runs` columns, so it survives this stage and stays available to
`usage.stats`. Then, once the task row itself is old enough to delete, its
run rows (and their usage) go with it.

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `days` | integer | `30` | no | Delete rows older than this many days (0 = keep forever). |
| `min_tasks` | integer | `1000` | no | Always keep at least this many newest task rows. |
| `vacuum` | boolean | `true` | no | VACUUM + WAL checkpoint after a prune that deleted rows. |

## ReposSettings

Repo-local task catalog settings (`[repos]`).

`known` seeds the daemon's persisted known-repos registry at startup (the
registry survives config edits); `active` selects the daemon-wide active repo
by name at startup. `base_dir`, when set, is scanned for repositories so
cloned checkouts register themselves without a config edit. Repo task files
live in `<repo>/.favetto/tasks/`.

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `known` | RepoSeed[] | `[]` | no | Repositories to register at startup, by daemon-host path. |
| `active` | string (optional) | — | no | The active repo's name at startup; unset means the global catalog. |
| `base_dir` | string (optional) | — | no | Directory whose repositories are discovered automatically. `~`-expanded; any child carrying a `.favetto/` directory is registered. Unset disables scanning. Discovered repos are reclaimed when their directory vanishes; `known`/`repos.add` entries are pinned. |
| `depth` | integer | `1` | no | How many directory levels below `base_dir` to scan. `1` (the default) means immediate children; `2` also scans `org/repo` nesting. |

## RepoSeed

One `[[repos.known]]` entry: a repository path and an optional display name.

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `path` | string | — | yes | Absolute (or `~`-expanded) path to the repository on the daemon host. |
| `name` | string (optional) | — | no | Display name used as the namespace key; defaults to the directory basename. |

## McpSettings

MCP server registry seed (`[mcp]`).

Every entry is operator-authored and therefore treated as pre-approved. The
daemon copies seeds into its persisted registry at startup; later app edits
survive restarts. Favetto never installs or upgrades the server.

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `servers` | McpServerSeed[] | `[]` | no |  |

## McpServerSeed

One `[[mcp.servers]]` entry.

`env`/`headers` hold literal secret values (stored by the daemon in its
`0600` secrets file, not in this config file); `env_from`/`headers_from` map
the entry name to the daemon env var carrying the value.

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `name` | string | — | yes | Unique registry name, also used as the ACP server name. |
| `transport` | McpTransport | `stdio` | no | `stdio` (default), `http`, or `sse`. |
| `command` | string (optional) | — | no | Stdio executable; a bare name is resolved on `PATH` at seed time. |
| `args` | string[] | `[]` | no |  |
| `env` | Map<String, string> | `{}` | no | Literal environment values. |
| `env_from` | Map<String, string> | `{}` | no | Environment entries read from a daemon env var (name -> source var). |
| `url` | string (optional) | — | no | HTTP/SSE endpoint. |
| `headers` | Map<String, string> | `{}` | no | Literal HTTP header values. |
| `headers_from` | Map<String, string> | `{}` | no | HTTP header entries read from a daemon env var (name -> source var). |
| `enabled` | boolean | `true` | no |  |
| `approved` | boolean | `true` | no | Operator-authored seeds are pre-approved; consent can still be revoked through `mcp.approve`. |

## McpTransport

How Favetto connects to an MCP server.

Stdio is mandatory for every ACP agent; HTTP and SSE are opt-in through the
agent's advertised `mcpCapabilities`.

string

## ExecutorSettings

How the executor runs tasks: concurrency and directory/worktree isolation.

With `parallel = true` each task gets a git worktree when its working
directory is inside a repository (so tasks can run side by side without
stepping on each other). Tasks that do **not** run in a worktree are
serialized per working directory.

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `parallel` | boolean | `false` | no | Run multiple tasks at once (default false: one at a time). |
| `max_concurrency` | integer | `4` | no | Maximum concurrent tasks when `parallel` is true. |
| `worktree` | boolean | `true` | no | Give each task its own `git worktree` when it runs in a repository. |
| `worktree_dir` | string (optional) | — | no | Where worktrees are created: absolute, or relative to the repo root. Defaults to `<data_dir>/worktrees`. |
| `worktree_retention` | WorktreeRetentionSettings | `{"days":30,"min_worktrees":0}` | no | Retention policy for worktrees left on disk. |
| `keep_worktree` | boolean | `false` | no | Keep worktrees after the task finishes (default false) so a completed task cleans up after itself. Set true to keep the agent's branch/changes for inspection; the retention sweep still reclaims kept worktrees after `worktree_retention.days`. |
| `max_output_bytes` | integer | `262144` | no | Cap on the stored `task.output` blob, in bytes (default 256 KiB). Longer output is truncated head+tail and flagged. |
| `detect_awaiting_input` | boolean | `true` | no | Detect when a running agent is blocked waiting for user input and surface it as `awaiting_input` on the task (default true). |
| `awaiting_input_quiet_ms` | integer | `8000` | no | How long the PTY must be quiet (ms) before the generic prompt detector fires (default 8000). Agent-specific detectors ignore this. |
| `stale_run` | StaleRunPolicy | `fail` | no | Startup policy for a task left in flight by a previous daemon: `"fail"` (default) marks it failed, `"retry"` re-enqueues it for a fresh attempt. |
| `retry` | RetrySettings | `{"max_attempts":1,"backoff":"exponential","initial_ms":5000,"max_ms":300000,"retry_on":["infrastructure","timeout"]}` | no | Automatic retry policy for retryable failures. Disabled by default. |

## WorktreeRetentionSettings

Worktree retention policy. At daemon start (and every six hours) recorded
worktrees whose owning task is finished and older than `days` are removed,
along with their `favetto/*` branch; orphans whose task row was already
pruned are reclaimed regardless of age. `days = 0` keeps opt-in worktrees
forever. Active (`pending`/`running`/`awaiting_input`) tasks are never
touched.

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `days` | integer | `30` | no | Remove a finished task's worktree older than this many days (0 = keep forever). |
| `min_worktrees` | integer | `0` | no | Always keep at least this many newest finished worktrees. |

## StaleRunPolicy

What the startup reconciler does with a task whose in-flight run was left
behind by a previous daemon instance.

string

## RetrySettings

Automatic retry policy for retryable task failures (`[executor.retry]`).

Retries are opt-in: the default `max_attempts = 1` means a task runs exactly
once. When enabled, after a failed attempt whose [`FailureKind`] is listed in
`retry_on`, the executor re-enqueues the task for `attempt + 1` after the
backoff, recording one run per attempt. Agent and invalid-input failures are
excluded from the default `retry_on`, so a genuine failure is never retried
on its own.

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `max_attempts` | integer | `1` | no | Total execution attempts allowed per task, including the first. `1` (default) disables automatic retries. |
| `backoff` | Backoff | `exponential` | no | How the delay between attempts grows. |
| `initial_ms` | integer | `5000` | no | Base delay before the second attempt, in milliseconds. |
| `max_ms` | integer | `300000` | no | Upper bound on any single backoff delay, in milliseconds. |
| `retry_on` | FailureKind[] | `["infrastructure","timeout"]` | no | Failure kinds eligible for automatic retry. Defaults to infrastructure and timeout faults; `agent` and `invalid_input` should stay out. |

## Backoff

How the delay between automatic retry attempts grows.

string

## FailureKind

Why a task ended in failure, so a controller can distinguish a genuine
agent failure from an infrastructure fault.

string

## TuiSettings

TUI-only settings. The daemon parses but ignores this section.

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `sound` | SoundSettings | `{"enabled":true,"player":"auto","min_interval_ms":400,"only_when_unfocused":false,"events":{"attention":"none","awaiting_input":"attention","task_failed":"failure","task_finished":"success","task_started":"none"}}` | no |  |
| `editor` | string (optional) | — | no | Editor command for `e` on a Catalog task. Unset -> `$VISUAL`, then `$EDITOR`, then `vi`. May include arguments (e.g. `code --wait`). |
| `theme` | string | `opencode` | no | Built-in (`opencode`, `catppuccin`) or user theme name. A user theme is loaded from `themes/<name>.json` in the config or `.favetto/themes` directory. Overridable with `FAVETTO_THEME_NAME`. |
| `theme_mode` | string | `system` | no | Theme mode: `system` (derive from the terminal), `dark`, or `light`. Overridable with `FAVETTO_THEME`. |
| `show_header` | boolean | `true` | no | Show the tab bar. Default true. |
| `show_footer` | boolean | `true` | no | Show the status line. Default true. |

## SoundSettings

Client-side sound notifications for the TUI.

Precedence is CLI flags (`--sound`/`--no-sound`/`--sound-command`) over the
`FAVETTO_SOUND*` environment variables over this section over built-in
defaults. Sounds are played on the machine running `favetto tui`.

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `enabled` | boolean | `true` | no | Master switch (default true). |
| `player` | string | `auto` | no | `"auto"` (detect a player on `PATH`), `"bell"`, or `"command"`. |
| `command` | string (optional) | — | no | Template used when `player = "command"`; `{file}` is the sound path. |
| `sound_dir` | string (optional) | — | no | Directory for relative `.wav` event values. `~` is expanded. |
| `min_interval_ms` | integer | `400` | no | Minimum gap between cues; bursts inside it are coalesced (failure wins). |
| `only_when_unfocused` | boolean | `false` | no | Only play while the terminal is unfocused (best-effort focus reporting). |
| `events` | Map<String, string> | `{"attention":"none","awaiting_input":"attention","task_failed":"failure","task_finished":"success","task_started":"none"}` | no | Cue key -> sound spec (`success`/`failure`/`attention`/`started`/`bell`, a `.wav` path, or `none`). Unset keys use the built-in default. |

## AuthSettings

Authentication settings: the username/password login policy.

Login attempts are rate-limited per `(username, client)`; `require_tls`
rejects non-loopback logins that did not arrive over HTTPS (directly or via
a proxy that sets `X-Forwarded-Proto: https`).

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `login_rate_limit` | integer | `5` | no | Failed `POST /auth/login` attempts allowed per `(username, client)` in `login_window_secs` before the client is locked out (0 disables the limit). |
| `login_window_secs` | integer | `300` | no | Window, in seconds, over which `login_rate_limit` failures accumulate. |
| `require_tls` | boolean | `false` | no | Reject non-loopback logins that did not arrive over HTTPS. |

