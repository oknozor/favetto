# Environment variables

favetto reads a small set of environment variables for paths, client behaviour,
and integration secrets. Anything not listed here is configured in
`config.toml`; see the [configuration reference](./config).

## Paths and config

| Variable | Used by | Description |
|----------|---------|-------------|
| `FAVETTO_CONFIG` | daemon, TUI | Path to the config file. Overrides the default `~/.config/favetto/config.toml`; `--config` overrides both. |
| `FAVETTO_DATA_DIR` | daemon, TUI | Directory for SQLite and the bearer token. Defaults to `~/.local/share/favetto`. |
| `FAVETTO__SECTION__KEY` | daemon, TUI | Override any config key, e.g. `FAVETTO__DAEMON__LISTEN=127.0.0.1:9000`. The separator is a double underscore. |
| `FAVETTO_URL` | TUI | Default remote WebSocket URL used when `--remote` is not given. |

## Client behaviour

| Variable | Used by | Description |
|----------|---------|-------------|
| `FAVETTO_THEME` | TUI | `dark` or `light`. Overrides terminal auto-detection. |
| `FAVETTO_PLAIN_ICONS` | TUI | Set to `1` for an ASCII fallback when the default folder/file emoji render double-width. |
| `FAVETTO_SOUND` | TUI | `1/true/on/yes` or `0/false/off/no/none`; overrides `[tui.sound].enabled`. |
| `FAVETTO_SOUND_DIR` | TUI | Overrides `[tui.sound].sound_dir`. |
| `FAVETTO_SOUND_COMMAND` | TUI | Overrides the sound player command. |
| `FAVETTO_NO_AGENT_WRAP` | daemon | Set to skip the `favetto __agent-exec` supervisor wrapper and launch agents directly (debugging only). |

## Secrets and integrations

| Variable | Used by | Description |
|----------|---------|-------------|
| `GITHUB_WEBHOOK_SECRET` | daemon | Default signing secret for `/webhooks/github` when `[webhook.github]` has no `secret`/`secret_env`. |
| `LINEAR_WEBHOOK_SECRET` | daemon | HMAC secret for the Linear webhook receiver. |
| `OPENCODE_MODELS_URL` | daemon | Overrides the model catalog URL (`https://models.dev/api.json`). |

`[git].passphrase_env` names any variable of your choosing that holds a signing
passphrase; the docs use `FAVETTO_GIT_SIGNING_PASSPHRASE` as the example. The
value is read from the daemon's environment and forwarded to the agent.

## Logging

| Variable | Used by | Description |
|----------|---------|-------------|
| `RUST_LOG` | all | `tracing` filter, e.g. `RUST_LOG=favetto=debug`. Defaults to `favetto=info`. |
