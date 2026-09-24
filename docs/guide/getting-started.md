# Getting started

favetto is a single binary that runs in two roles: a long-lived **daemon**
(scheduler, task queue, persistence, remote API) and a **TUI client**. This page
starts both and attaches the client. It takes about two minutes.

If you have not built favetto yet, start with [Installation](./installation).

## 1. Run the daemon

The daemon owns the database, watches the task catalog, and exposes the API. It
runs in the foreground, so keep it in its own terminal:

```bash
favetto daemon
```

By default it listens on a Unix socket (`/tmp/favetto.sock`) for local attach and
on `127.0.0.1:7878` for authenticated remote attach. Both settings are
overridable with `--socket` / `--listen`, or from
`~/.config/favetto/config.toml`.

```text
INFO favetto::daemon: listening on unix:/tmp/favetto.sock
INFO favetto::daemon: websocket API on 127.0.0.1:7878
```

## 2. Attach the TUI

In a second terminal, attach to the daemon over the local socket:

```bash
favetto tui
```

![favetto TUI — Catalog tab](/screenshots/tui-catalog.png)

*The Catalog tab: folder tree + preview pane.*

When the TUI opens you are on the **Catalog** tab. Folders show the structure of
your task directory, `Enter` starts the highlighted task, and the right pane
previews its raw Markdown. Press `?` for the full keybinding list and `w` for the
workflow graph.

## 3. Or attach remotely

The same client attaches over the network. The daemon writes a bearer token to
`~/.local/share/favetto/token` on first start:

```bash
favetto tui --remote ws://127.0.0.1:7878/rpc \
    --token-file ~/.local/share/favetto/token
```

For a short-lived code instead of copying the token, ask the daemon for one and
pass it with `--pair-code`:

```bash
favetto pair
# pairing code (valid 60s): 123456
# attach with: favetto tui --remote ws://HOST:7878/rpc --pair-code 123456

favetto tui --remote ws://127.0.0.1:7878/rpc --pair-code 123456
```

See [Remote access](./remote-access) for the full flow.

## Appearance

The TUI selects a dark or light theme from the terminal background (an OSC 11
query, then `COLORFGBG`, then dark). Override it with `FAVETTO_THEME=dark` or
`FAVETTO_THEME=light`; there is no theme file or picker.

## Next steps

- [Your first task](./first-task) — create a task and run it end to end.
- [Catalog](./catalog) — how task files are discovered and started.
- [Embedded agents](./agents) — configure the agent CLIs favetto drives.
- [Configuration](./configuration) — daemon, executor, git, and sound settings.
