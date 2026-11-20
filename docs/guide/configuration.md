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

[agents.opencode]              # every agent is an ACP server
command = "opencode"
args = ["acp"]
# permission = "ask"           # "ask" (default), "allow", or "deny"

[daemon]                       # defaults, overridable by CLI flags
listen = "127.0.0.1:7878"
socket = "/tmp/favetto.sock"
# tasks_dir = "~/.config/favetto/tasks"

[repos]                        # repo-local task catalogs (see Catalog)
# active = "favetto"
# known = [{ path = "~/code/favetto" }]
```

Where to read next:

- [Configuration reference](../reference/config) — every section, field, type,
  default, and enum value, generated from the source.
- [Embedded agents](./agents) — configuring agent invocations.
- [Parallel execution and worktrees](./parallel-worktrees) — `[executor]`.
- [Git and commit signing](./git-signing) — `[git]`.
- [Webhooks and hooks](./webhooks) — `[webhook.github]`.
- [Catalog](./catalog) — repo-local task catalogs (`[repos]`).
- [TUI](./tui) — `[tui.sound]`.
- [Environment variables](../reference/environment) — the overrides and secrets.

## Full example

The annotated [`config.example.toml`](https://github.com/oknozor/favetto/blob/main/config.example.toml)
in the repository is the canonical starting point. It is included here verbatim
so it cannot drift from the code:

<<< ../../config.example.toml{toml}
