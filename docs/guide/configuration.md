# Configuration

favetto reads a global config file (parsed with the `config` crate, located via
the `dirs` crate) at **`~/.config/favetto/config.toml`** — override with
`--config <path>` or `$FAVETTO_CONFIG`. It declares the external agents and daemon
defaults:

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

Data (SQLite + bearer token) lives in **`~/.local/share/favetto`** (or
`$FAVETTO_DATA_DIR`). Settings resolve in the order CLI flag → config file →
built-in default.

GitHub webhook secrets resolve from `[webhook.github]` (`secret`, else the env
var named by `secret_env`, else `GITHUB_WEBHOOK_SECRET`); Linear reads
`LINEAR_WEBHOOK_SECRET`. See [Webhooks & hooks](./webhooks).

## Full example

The annotated [`config.example.toml`](https://github.com/oknozor/favetto/blob/main/config.example.toml)
in the repository is the canonical reference. It is included here verbatim so it
can never drift from the code:

<<< ../../config.example.toml{toml}
