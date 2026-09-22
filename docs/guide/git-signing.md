# Git and commit signing

Agent CLIs run in favetto-owned PTYs. If your git config has
`commit.gpgsign = true`, a `git commit` made by an agent would invoke gpg or ssh
and wait for a pinentry/askpass prompt nobody can answer — an interactive
session stalls and a headless run hangs until it is torn down.

The `[git]` section makes agent commits **prompt-free by default** and lets you
opt into signed agent commits:

```toml
# ~/.config/favetto/config.toml
[git]
signing = "off"          # "off" (default) | "ssh" | "gpg"
# user_name = "favetto agent"
# user_email = "agent@favetto.local"
# signing_key = "~/.config/favetto/agent_signing.pub"  # required for ssh/gpg
# passphrase_env = "FAVETTO_GIT_SIGNING_PASSPHRASE"
# passphrase_command = ["secret-tool", "lookup", "service", "favetto-git-signing"]
```

- **Default (`off`)** injects `commit.gpgsign = false`, so an agent commit can
  never block. No configuration is required to fix the hang.
- **`ssh`** (recommended for opt-in signing) sets `commit.gpgsign = true`,
  `gpg.format = ssh`, and `user.signingkey = <signing_key>`. Use a dedicated,
  passphrase-less key or one already loaded in `ssh-agent` and the commit is
  genuinely prompt-free. With a passphrase source, favetto writes a `0700`
  askpass script and sets `SSH_ASKPASS`/`GIT_ASKPASS` plus
  `SSH_ASKPASS_REQUIRE=force`.
- **`gpg`** sets `commit.gpgsign = true`, `gpg.format = openpgp`, and
  `user.signingkey = <signing_key>`. With a passphrase source it generates a
  `gpg.program` wrapper that passes the secret on fd 3 with
  `--pinentry-mode loopback`; the agent's `gpg-agent` must have
  `allow-loopback-pinentry` enabled, otherwise signing fails fast rather than
  hanging.

Passphrases come from `passphrase_env` (read from the daemon's environment and
forwarded to the agent) or `passphrase_command` (an argv run at signing time by
the generated wrapper, so the secret never appears in argv, the config file, or
the child's environment). Never put a passphrase in `config.toml`.

## Precedence

The mechanism is git's *environment config* (`GIT_CONFIG_COUNT`,
`GIT_CONFIG_KEY_<i>`, `GIT_CONFIG_VALUE_<i>`) plus `GIT_AUTHOR_*` /
`GIT_COMMITTER_*`, so **your real global/repo/worktree config is never modified**
and nothing is written into the worktree. It affects agent processes only, and
`[agents.x.env]` values are applied before `[git]`, so `[git]` is authoritative.
It requires **git ≥ 2.31**.

The most specific level wins: a task's `sign` header → `[agents.<name>.git]` →
`[git]`. An agent can override any field of the global section:

```toml
[agents.opencode.git]
signing = "ssh"
signing_key = "~/.ssh/agent_ed25519.pub"
```

And a single task can override everything for its own run:

```md
sign = "ssh"
---
Make a signed commit.
```

See the [task file format reference](../reference/tasks#header-keys) for the
`sign` header and the [configuration reference](../reference/config#gitsettings)
for every `[git]` field.
