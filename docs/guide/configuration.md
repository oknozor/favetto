# Configuration

favetto reads a global config file at **`~/.config/favetto/config.toml`** —
override it with `--config <path>` or `$FAVETTO_CONFIG`. Data (SQLite and the
bearer token) lives in **`~/.local/share/favetto`** (`$FAVETTO_DATA_DIR`).

Settings resolve in the order **CLI flag → config file → built-in default**, and
any `FAVETTO__SECTION__KEY` environment variable overrides the file. Every
section is optional.

```toml
# ~/.config/favetto/config.toml
[agent]
default = "opencode"           # default external agent (a key in [agents.*])

[agents.opencode]              # external coding agents
command = "opencode"
args = []
prompt_args = ["--prompt", "{prompt}"]   # seed an interactive session
submit_prompt = true                     # press Enter so the seeded prompt is sent
headless_args = ["run", "--auto", "{prompt}"]   # unattended runs without a model
run_args = ["run", "--model", "{provider}/{model}", "--auto", "--format", "json", "{prompt}"]
resume_args = ["--session", "{session_id}"]     # reopen a run's session
session_id_json_key = "sessionID"               # captured from run JSON output

[daemon]                       # defaults, overridable by CLI flags
listen = "127.0.0.1:7878"
socket = "/tmp/favetto.sock"
tasks_dir = "tasks"
```

Where to read next:

- [Configuration reference](../reference/config) — every section, field, type,
  default, and enum value, generated from the source.
- [Embedded agents](./agents) — configuring agent invocations.
- [Parallel execution and worktrees](./parallel-worktrees) — `[executor]`.
- [Git and commit signing](./git-signing) — `[git]`.
- [Webhooks and hooks](./webhooks) — `[webhook.github]`.
- [TUI](./tui) — `[tui.sound]`.
- [Environment variables](../reference/environment) — the overrides and secrets.

## Full example

The annotated [`config.example.toml`](https://github.com/oknozor/favetto/blob/main/config.example.toml)
in the repository is the canonical starting point. It is included here verbatim
so it cannot drift from the code:

<<< ../../config.example.toml{toml}
