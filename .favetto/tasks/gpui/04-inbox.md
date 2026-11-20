agent = "opencode"
provider = "opencode-go"
model = "deepseek-v4.1-flash"
worktree = false
permission = "allow"
needs = "gpui/03-shell:succeeded"
---

You are implementing **step 04 of 11** of the favetto GPUI desktop client.

Read first:
- `docs/design/gpui-client.md` (Inbox surface, Von Restorff, Doherty, Peak-End).
- `gpui/README.md`, `gpui/src/state.rs`, `gpui/src/transport.rs`.

## Goal

Build the landing surface: **"What needs me now?"** — waiting sessions first,
recent runs second, updating live.

## Work

1. Load `agents.list` and `tasks.list` on connect and on `task.updated`,
   `agent.state`, `agent.event` pushes.
2. **Group 1 — Needs you**: sessions with `awaiting_input`, plus failed tasks
   from the last 24 h. Each row shows: status icon + label, task name, agent,
   attempt, elapsed time, and (for permissions) the request message.
   Awaiting rows use the accent stripe + icon (not color alone).
3. **Group 2 — Recent**: the newest tasks, one line each (status, name, repo,
   duration, usage cost when known).
4. Sorting: needs-you by oldest waiting first (fair queue); recent by newest.
5. Actions: `Enter` opens the task detail (wired in step 06); `r` refreshes;
   empty state explains how to start a task.
6. Live updates: a new awaiting request appears within one SSE tick without a
   manual refresh. Keep the scroll position when rows update.
7. Loading skeleton for the first fetch; error state with retry.

## Definition of done

- `cd gpui && cargo check && cargo test && cargo fmt --check && cargo clippy -- -D warnings`.
- Opening the app against a daemon with a blocked session shows it in
  "Needs you" first.
- No `unwrap` on task/session fields; unknown status degrades to a neutral row.

## Commit

- After the definition of done is satisfied, create a git commit for this task before reporting completion.
- Review `git status` and the diff; stage only changes made for this task. Do not include pre-existing or unrelated changes, and never amend an existing commit.
- If task changes overlap pre-existing edits and cannot be cleanly isolated, stop and ask before committing.
- Use a concise commit message, e.g. `feat(gpui): build inbox surface`.
