# Installation

favetto is a single Rust binary. You need a Rust toolchain and at least one
external coding-agent CLI for it to orchestrate.

## Prerequisites

- **Rust** 1.94 or newer (edition 2021): <https://rustup.rs>.
- **An agent CLI** on your `PATH` — for example
  [opencode](https://opencode.ai), Claude Code, pi, or Mistral Vibe. favetto
  never implements its own agent loop, so a task cannot start without one.

## Build from source

```bash
git clone https://github.com/oknozor/favetto
cd favetto
cargo build --release
```

The binary lands at `target/release/favetto`. To put it on your `PATH`:

```bash
cargo install --path crates/favetto
```

## Verify

```bash
favetto --version
```

```text
favetto 0.1.0
```

::: tip No configuration required to start
The four built-in agents (opencode, claude, pi, vibe) are always registered, so
an empty config exposes them. Add a `[agents.*]` section only when you want to
override an invocation — see [Embedded agents](./agents).
:::

## Run as a systemd user service

Running the daemon in a foreground terminal is fine for trying favetto out, but
for a persistent setup install it as a **systemd user service** so it starts at
login, restarts on failure, and logs to the journal. A user unit (rather than a
system one) matches favetto's per-user paths (`~/.config/favetto`,
`~/.local/share/favetto`) and the per-user agent CLIs it launches.

From a checkout of the repository:

```bash
./scripts/install.sh
```

The script:

1. installs the `favetto` binary with `cargo install --path crates/favetto --locked`,
2. seeds `~/.config/favetto/config.toml` from `config.example.toml` when missing,
3. creates the private `~/.config/favetto/env` secrets file (mode 600) and, when
   `gh` is authenticated, seeds `GITHUB_TOKEN` from `gh auth token`
   (pass `--no-github-token` to skip; an existing value is never overwritten),
4. installs `packaging/systemd/favetto.service` to
   `~/.config/systemd/user/favetto.service`,
5. enables the unit, reloads the user manager, and **restarts** the service so
   it runs with the freshly installed binary and environment.

It never uses `sudo`. Pass `--no-start` to install without enabling the unit.

::: tip Secrets live in an env file, not the unit
Scheduled and action tasks can interpolate secrets from the daemon's
environment (for example a `poll_issues` gate that sends
`Authorization = "Bearer $GITHUB_TOKEN"`). The shipped unit loads
`~/.config/favetto/env` through `EnvironmentFile=`, so the token stays out of
both the unit and the config. That file is read **only when the unit starts**;
after editing it, apply the change with:

```bash
systemctl --user restart favetto
```

The unit marks the file optional (`EnvironmentFile=-…`), so the daemon still
starts when it has not been created.
:::

::: warning Agent CLIs must be on the unit's PATH
systemd's default `PATH` does not include `~/.cargo/bin` or `~/.local/bin`, and
favetto resolves agent CLIs from `PATH`. The shipped unit sets
`Environment=PATH=%h/.cargo/bin:%h/.local/bin:/usr/local/bin:/usr/bin:/bin`; if
your agent CLI lives elsewhere, edit that line in
`~/.config/systemd/user/favetto.service` and run
`systemctl --user daemon-reload`. The install script warns when it finds none of
`opencode`, `claude`, `pi`, or `vibe`.
:::

The default task catalog lives at `~/.config/favetto/tasks` (independent of the
unit's `WorkingDirectory`). To point the daemon at another catalog, set
`[daemon].tasks_dir` in `~/.config/favetto/config.toml`.

Manage the daemon with:

```bash
systemctl --user status favetto     # is it running?
systemctl --user restart favetto    # pick up a new binary or config
systemctl --user stop favetto       # graceful shutdown (SIGTERM)
systemctl --user disable favetto    # do not start at login
```

Follow the logs with:

```bash
journalctl --user -u favetto -f
```

Then attach the TUI as usual with `favetto tui`. To remove the service:

```bash
./scripts/install.sh --uninstall           # keeps ~/.config/favetto and ~/.local/share/favetto
./scripts/install.sh --uninstall --purge   # also deletes config and data
```

If you prefer to wire it up by hand, copy `packaging/systemd/favetto.service`
to `~/.config/systemd/user/`, adjust `ExecStart` to the absolute path of the
binary, then `systemctl --user daemon-reload && systemctl --user enable --now favetto`.

Next: [Getting started](./getting-started).
