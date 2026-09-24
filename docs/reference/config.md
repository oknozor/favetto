# Configuration reference

Generated from `crates/favetto-core/src/config.rs` by the docs generator. Do not edit by hand.

Every section is optional; favetto falls back to built-in defaults for anything you omit. Values resolve in the order **CLI flag → config file → built-in default**, and every `FAVETTO__SECTION__KEY` environment variable overrides the file.

## FavettoConfig

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `agent` | AgentSettings | `{}` | no | Default external agent used for catalog tasks and new agent sessions. |
| `agents` | Map<String, AgentConfig> | `{}` | no | External coding agents by name (e.g. `claude`, `opencode`, `pi`, `vibe`). |
| `auth` | AuthSettings | `{"ticket_ttl_secs":30}` | no | Short-lived authentication ticket settings. |
| `daemon` | DaemonSettings | `{"data_dir":null,"listen":null,"retention":{"days":30,"min_tasks":1000,"vacuum":true},"socket":null,"tasks_dir":null}` | no | Daemon defaults (overridable by CLI flags). |
| `executor` | ExecutorSettings | `{"awaiting_input_quiet_ms":8000,"detect_awaiting_input":true,"keep_worktree":false,"max_concurrency":4,"max_output_bytes":262144,"parallel":false,"retry":{"backoff":"exponential","initial_ms":5000,"max_attempts":1,"max_ms":300000,"retry_on":["infrastructure","timeout"]},"stale_run":"fail","worktree":true,"worktree_retention":{"days":30,"min_worktrees":0}}` | no | Task executor concurrency / isolation. |
| `git` | GitSettings | `{}` | no | Non-interactive git provisioning for agent processes. Defaults to `signing = "off"`, which forces `commit.gpgsign = false` so an agent commit can never block on an interactive pinentry/askpass prompt. |
| `tui` | TuiSettings | `{"sound":{"enabled":true,"events":{"attention":"none","awaiting_input":"attention","task_failed":"failure","task_finished":"success","task_started":"none"},"min_interval_ms":400,"only_when_unfocused":false,"player":"auto"}}` | no | Client-side TUI settings. Ignored by the daemon, which reads the same file. |
| `web` | WebSettings | `{"dir":"","enabled":true,"heartbeat_secs":15}` | no | Web client (embedded SPA) settings. |
| `webhook` | WebhookSettings | `{"github":{"enabled":false,"rules":[]}}` | no | Webhook trigger rules (currently GitHub). |

## AgentConfig

An external coding-agent CLI (opencode, Claude Code, pi, Mistral Vibe, …).

`args` is the interactive invocation used by the embedded terminal panel.
`prompt_args`, when present, is appended (with `{prompt}` substituted) so a
session can start with the task prompt. `headless_args` drives unattended
catalog-task runs; `{prompt}` is substituted there too. When a prompt is given
but no `prompt_args`/`headless_args` are configured, the prompt is written to
the process's stdin.

Model selection uses `run_args` instead of `headless_args` when a task sets a
`model`. Both support `{prompt}`, `{provider}`, and `{model}` placeholders, so
the flag can be placed where the CLI expects it (e.g. after a `run`
subcommand). `resume_args` is the interactive invocation for reopening a
previously run session and supports `{session_id}`. When `session_id_json_key`
is set, the agent's session id is read from a run's line-delimited JSON output
under that key (e.g. `"sessionID"`) and stored on the task.

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `args` | string[] | `[]` | no | Interactive arguments. |
| `command` | string | — | yes | Executable to launch (looked up on `PATH`). |
| `cwd` | string (optional) | — | no | Working directory (defaults to the daemon's cwd). |
| `env` | Map<String, string> | `{}` | no | Extra environment variables for the process. |
| `git` | GitSettings (optional) | — | no | Per-agent override of the global `[git]` section. Only the fields set here replace the global values; the rest are inherited. |
| `headless_args` | string[] (optional) | — | no | Arguments for unattended task runs without a `model`. `{prompt}` is substituted. |
| `hooks` | boolean (optional) | — | no | claude only: inject favetto's HTTP hooks through a per-launch `--settings` file. Unset keeps the built-in default. |
| `interactive_model_args` | string[] (optional) | — | no | Interactive arguments used when a model is selected (e.g. for a one-shot session); `{provider}` and `{model}` are substituted. Lets an agent reach a path that accepts a model when its plain `args` do not. |
| `output_format` | string (optional) | — | no | Structured stdout format for headless runs and final summaries, e.g. `"opencode-json"`, `"claude-stream-json"`, `"pi-json"`, `"pi-rpc"`, `"vibe-streaming"`, or `"plain-jsonl"`. Unset keeps the default `{ "text": raw }` output. A custom (`configurable`) agent only has a parser for `"plain-jsonl"`. |
| `prompt_args` | string[] (optional) | — | no | Arguments appended to the interactive command when starting with a prompt. |
| `resume_args` | string[] (optional) | — | no | Interactive arguments for reopening an existing session; `{session_id}` is substituted. |
| `run_args` | string[] (optional) | — | no | Arguments for unattended task runs with a `model`; `{prompt}`, `{provider}`, and `{model}` are substituted. |
| `server` | string (optional) | — | no | opencode only: which server to observe. `"managed"` (favetto-owned `serve`), `"background"` (the registered background service), or a URL. |
| `session_id_json_key` | string (optional) | — | no | Key under which a run's line-delimited JSON output carries the agent's session id (e.g. `"sessionID"`). Unset disables capture. |
| `state` | AgentStateMode (optional) | — | no | How to observe the agent's live state. Unset (or `auto`) keeps the built-in default for the agent; `none` forces the screen fallback. |
| `submit_prompt` | boolean (optional) | — | no | After starting interactively with a prompt via `prompt_args`, send Enter to submit it once the agent's UI has settled (some agents, e.g. opencode, only pre-fill the input with `--prompt`). `None` leaves the built-in default in place. |
| `type` | string (optional) | — | no | Implementation discriminator: `opencode`, `claude`, `pi`, `vibe`, or `configurable`. Omit to use the built-in whose name matches the entry (or the template-only fallback for a custom name). |

## AgentSettings

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `default` | string (optional) | — | no | Name of the default external agent (a key in `[agents.*]`). When set, catalog tasks run through this agent unless a task names its own. |

## AgentStateMode

How an agent's live state is observed (see
`docs/design/agent-state-adapters.md`).

`auto` keeps the built-in default for the agent; `none` disables structured
observation entirely (the debounced screen heuristic still runs); `stdout`
parses the CLI's line-delimited JSON output; `server` observes a long-lived
HTTP/SSE endpoint (opencode); `hooks` observes via per-launch HTTP hooks
(claude). A custom agent has no built-in transport, so anything but `auto`
is purely declarative for it.

string

## AuthSettings

Short-lived, single-use tickets for header-less clients (browser
WebSocket/SSE auth). The ticket is minted over authenticated HTTP and then
presented in a query string, where an `Authorization` header cannot go.

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `ticket_ttl_secs` | integer | `30` | no | Ticket lifetime, in seconds. |

## Backoff

How the delay between automatic retry attempts grows.

string

## DaemonSettings

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `data_dir` | string (optional) | — | no |  |
| `listen` | string (optional) | — | no |  |
| `retention` | RetentionSettings | `{"days":30,"min_tasks":1000,"vacuum":true}` | no | Bounded database growth. On by default; `days = 0` keeps everything. |
| `socket` | string (optional) | — | no |  |
| `tasks_dir` | string (optional) | — | no |  |

## ExecutorSettings

How the executor runs tasks: concurrency and directory/worktree isolation.

With `parallel = true` each task gets a git worktree when its working
directory is inside a repository (so tasks can run side by side without
stepping on each other). Tasks that do **not** run in a worktree are
serialized per working directory.

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `awaiting_input_quiet_ms` | integer | `8000` | no | How long the PTY must be quiet (ms) before the generic prompt detector fires (default 8000). Agent-specific detectors ignore this. |
| `detect_awaiting_input` | boolean | `true` | no | Detect when a running agent is blocked waiting for user input and surface it as `awaiting_input` on the task (default true). |
| `keep_worktree` | boolean | `false` | no | Keep worktrees after the task finishes (default false) so a completed task cleans up after itself. Set true to keep the agent's branch/changes for inspection; the retention sweep still reclaims kept worktrees after `worktree_retention.days`. |
| `max_concurrency` | integer | `4` | no | Maximum concurrent tasks when `parallel` is true. |
| `max_output_bytes` | integer | `262144` | no | Cap on the stored `task.output` blob, in bytes (default 256 KiB). Longer output is truncated head+tail and flagged. |
| `parallel` | boolean | `false` | no | Run multiple tasks at once (default false: one at a time). |
| `retry` | RetrySettings | `{"backoff":"exponential","initial_ms":5000,"max_attempts":1,"max_ms":300000,"retry_on":["infrastructure","timeout"]}` | no | Automatic retry policy for retryable failures. Disabled by default. |
| `stale_run` | StaleRunPolicy | `fail` | no | Startup policy for a task left in flight by a previous daemon: `"fail"` (default) marks it failed, `"retry"` re-enqueues it for a fresh attempt. |
| `worktree` | boolean | `true` | no | Give each task its own `git worktree` when it runs in a repository. |
| `worktree_dir` | string (optional) | — | no | Where worktrees are created: absolute, or relative to the repo root. Defaults to `<data_dir>/worktrees`. |
| `worktree_retention` | WorktreeRetentionSettings | `{"days":30,"min_worktrees":0}` | no | Retention policy for worktrees left on disk. |

## FailureKind

Why a task ended in failure, so a controller can distinguish a genuine
agent failure from an infrastructure fault.

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
| `passphrase_command` | string[] (optional) | — | no | Command (argv list) whose stdout is the passphrase. Embedded in the generated wrapper script, so the secret never lands in the config file. |
| `passphrase_env` | string (optional) | — | no | Env var (in the daemon's environment) holding the passphrase. The value is forwarded to the agent; prefer this for interactive-daemon setups. |
| `signing` | GitSigning (optional) | — | no | `"off"` (default), `"ssh"`, or `"gpg"`. |
| `signing_key` | string (optional) | — | no | GPG key id/fingerprint, or SSH public-key path / literal key. |
| `user_email` | string (optional) | — | no | Commit author/committer email (`user.email` + the matching git env vars). |
| `user_name` | string (optional) | — | no | Commit author/committer name (`user.name` + `GIT_AUTHOR_NAME`/`GIT_COMMITTER_NAME`). |

## GitSigning

How favetto provisions `git` in an agent process.

`off` is the safe default: agent commits are explicitly unsigned so they can
never wait for a passphrase prompt nobody can answer. `ssh`/`gpg` opt into
signed agent commits with a dedicated identity/key.

`off` | `gpg` | `ssh`

## GithubFilter

Optional narrowing for a [`GithubRule`]. Unset fields match anything; all set
fields must match (AND).

`repo`, `author`, `base_ref`, and `head_ref` are globs; `labels_contains` is
any-of exact.

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `author` | string (optional) | — | no |  |
| `base_ref` | string (optional) | — | no |  |
| `head_ref` | string (optional) | — | no |  |
| `labels_contains` | string[] | — | no |  |
| `repo` | string (optional) | — | no |  |

## GithubRule

A single `[[webhook.github.rules]]` entry.

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `action` | string (optional) | — | no | Optional action; unset matches any action. |
| `enabled` | boolean | `true` | no |  |
| `event` | string | — | yes | `X-GitHub-Event` value (e.g. `"issues"`). Validated at startup. |
| `filter` | GithubFilter | `{}` | no |  |
| `name` | string | — | yes |  |
| `task` | string | — | yes | Catalog task name. Validated at startup. |

## GithubWebhookSettings

GitHub webhook receiver + trigger rules.

GitHub POSTs signed events to `/webhooks/github`; each matching rule enqueues
the named catalog task with a truncated summary of the event as its input.
Rules and the secret are read at daemon startup, so a restart is required after
editing them.

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `enabled` | boolean | `false` | no | Opt-in; disabled unless set. |
| `rules` | GithubRule[] | `[]` | no |  |
| `secret` | string (optional) | — | no | Literal secret (accepted but discouraged). |
| `secret_env` | string (optional) | — | no | Name of the env var holding the signing secret. |

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
| `backoff` | Backoff | `exponential` | no | How the delay between attempts grows. |
| `initial_ms` | integer | `5000` | no | Base delay before the second attempt, in milliseconds. |
| `max_attempts` | integer | `1` | no | Total execution attempts allowed per task, including the first. `1` (default) disables automatic retries. |
| `max_ms` | integer | `300000` | no | Upper bound on any single backoff delay, in milliseconds. |
| `retry_on` | FailureKind[] | `["infrastructure","timeout"]` | no | Failure kinds eligible for automatic retry. Defaults to infrastructure and timeout faults; `agent` and `invalid_input` should stay out. |

## SoundSettings

Client-side sound notifications for the TUI.

Precedence is CLI flags (`--sound`/`--no-sound`/`--sound-command`) over the
`FAVETTO_SOUND*` environment variables over this section over built-in
defaults. Sounds are played on the machine running `favetto tui`.

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `command` | string (optional) | — | no | Template used when `player = "command"`; `{file}` is the sound path. |
| `enabled` | boolean | `true` | no | Master switch (default true). |
| `events` | Map<String, string> | `{"attention":"none","awaiting_input":"attention","task_failed":"failure","task_finished":"success","task_started":"none"}` | no | Cue key -> sound spec (`success`/`failure`/`attention`/`started`/`bell`, a `.wav` path, or `none`). Unset keys use the built-in default. |
| `min_interval_ms` | integer | `400` | no | Minimum gap between cues; bursts inside it are coalesced (failure wins). |
| `only_when_unfocused` | boolean | `false` | no | Only play while the terminal is unfocused (best-effort focus reporting). |
| `player` | string | `auto` | no | `"auto"` (detect a player on `PATH`), `"bell"`, or `"command"`. |
| `sound_dir` | string (optional) | — | no | Directory for relative `.wav` event values. `~` is expanded. |

## StaleRunPolicy

What the startup reconciler does with a task whose in-flight run was left
behind by a previous daemon instance.

string

## TuiSettings

TUI-only settings. The daemon parses but ignores this section.

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `editor` | string (optional) | — | no | Editor command for `e` on a Catalog task. Unset -> `$VISUAL`, then `$EDITOR`, then `vi`. May include arguments (e.g. `code --wait`). |
| `sound` | SoundSettings | `{"enabled":true,"events":{"attention":"none","awaiting_input":"attention","task_failed":"failure","task_finished":"success","task_started":"none"},"min_interval_ms":400,"only_when_unfocused":false,"player":"auto"}` | no |  |

## WebSettings

Web client (embedded SPA) settings.

`dir` overrides the assets embedded in the binary with a directory on disk
(development); leave it empty to serve the embedded assets. `heartbeat_secs`
is the interval of the SSE keep-alive comments that defeat idle proxy
timeouts.

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `dir` | string | `` | no | Asset directory override; empty serves the embedded assets. |
| `enabled` | boolean | `true` | no | Serve the embedded web client from the daemon. |
| `heartbeat_secs` | integer | `15` | no | SSE keep-alive interval, in seconds. |

## WebhookSettings

Webhook trigger settings. Currently only GitHub is supported.

| Field | Type | Default | Required | Description |
|-------|------|---------|----------|-------------|
| `github` | GithubWebhookSettings | `{"enabled":false,"rules":[]}` | no |  |

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

