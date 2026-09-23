# Your first task

This walkthrough creates a task, runs it through an embedded agent, and watches
the result in the TUI. It assumes you have [installed favetto](./installation)
and have an agent CLI (for example opencode) on your `PATH`.

## 1. Write a task

A task is a Markdown file with a TOML header and a prompt body. Create
`tasks/hello.md` in the directory you will run the daemon from:

```md
agent = "opencode"
---
Create a file named hello.txt containing the single line `hello from favetto`,
then stop.
```

`agent` names the CLI that will do the work. You can also set a global default
in `~/.config/favetto/config.toml` with `[agent] default = "opencode"`, in which
case the header can be omitted. See [Embedded agents](./agents) for how the
agent is invoked.

## 2. Start the daemon

The daemon watches `tasks/` for changes, so it can be running before or after you
write the file:

```bash
favetto daemon
```

```text
INFO favetto::catalog_watch: loaded 1 task(s) from "tasks"
```

## 3. Attach the TUI

In a second terminal:

```bash
favetto tui
```

![favetto TUI — Catalog tab](/screenshots/tui-catalog.png)

*The Catalog tab lists `hello` under the task root.*

## 4. Find and start the task

The **Catalog** tab is a collapsible tree. `hello` may be nested inside a folder
row — press `↓` to move to it and `Space` (or `Enter` on the folder) to fold or
unfold. When `hello` is highlighted, the right pane previews its header and
prompt.

![favetto TUI — variables form](/screenshots/tui-vars-form.png)

*A task that declares `[[vars]]` opens a form first; `hello` has none, so it
starts immediately.*

Press `Enter` on the task. This is what happens:

1. The daemon emits **`task_idle`** — the task is queued.
2. The executor reserves a slot and emits **`task_started`**.
3. The agent runs the prompt headlessly in a PTY on the daemon.
4. When the process exits, the task is marked succeeded or failed and the daemon
   emits **`task_finished`**.

## 5. Watch the agent

The run opens in the **Agent** tab, which embeds the agent's live terminal.

![favetto TUI — Agent tab](/screenshots/tui-agent.png)

*The Agent tab: the embedded agent session, seeded with the task prompt.*

While the panel has keyboard focus it forwards every key to the agent. Press
`Ctrl+Y` to hand focus back to favetto, then `Ctrl+Q` to leave the panel without
stopping the session on the daemon.

## 6. Confirm the result

Return to the **Tasks** tab to see the run's status and its agent session title.

![favetto TUI — Tasks tab](/screenshots/tui-tasks.png)

*The Tasks tab lists recent runs with status, age, and session title.*

Switch to the **Events** tab to see the persisted event stream. Select a row to
pretty-print its JSON payload in the side panel.

![favetto TUI — Events tab](/screenshots/tui-events.png)

*The Events tab with the payload panel.*

You should see the three lifecycle events for `hello`:

| Event | Meaning |
|-------|---------|
| `task_idle` | The task was enqueued. |
| `task_started` | The agent began running. |
| `task_finished` | The run ended (check `success` in the payload for failure). |

::: tip It really ran
The agent ran in the task's working directory. `tasks/hello.md` requested a file
in the daemon's current directory, so `hello.txt` should now exist there.
:::

## Next steps

- [Catalog](./catalog) — subfolders, identities, live reload.
- [Variables and prompts](./variables-and-prompts) — prompt for input and
  template it into the prompt.
- [Schedules and dependencies](./schedules-and-dependencies) — chain tasks with
  `needs` and `spawn`.
- [Troubleshooting](./troubleshooting) — when a task will not start.
