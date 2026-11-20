agent = "opencode"
provider = "opencode-go"
model = "deepseek-v4.1-flash"
worktree = false
permission = "allow"
needs = "gpui/05-tasks:succeeded"
---

You are implementing **step 06 of 11** of the favetto GPUI desktop client.

Read first:
- `docs/design/gpui-client.md` (Task detail; Peak-End, Working Memory,
  Von Restorff).
- `gpui/README.md`, `gpui/src/{state,transport}.rs`.
- The wire types: `crates/favetto-wire/src/model.rs`,
  `crates/favetto-wire/src/agent_state.rs`, `crates/favetto-wire/src/rpc/messages.rs`.

## Goal

The core surface: understand and steer one run without leaving the app.

## Work

1. **Header (pinned)**: task name, status badge, agent, repo, attempt, elapsed
   time, session id (copyable), cost/tokens when known. Actions: Cancel,
   Retry (with confirmations), Copy output.
2. **Tabs**: Output, Events, Runs.
   - **Output**: the persisted `tasks.get` output, rendered as plain text
     (truncated notice when `truncated`), monospace, selectable, with a
     "follow tail" toggle.
   - **Events**: `tasks.events` backfill + live `agent.event` pushes, ordered by
     `seq`. Render text deltas, reasoning (muted), tool start/finish/badges,
     usage, errors. Group consecutive deltas into one message block.
   - **Runs**: `tasks.runs` table (attempt, status, exit code, duration, usage).
3. **Permission banner**: when the session reports `awaiting_input`, pin a
   banner with the request message and its options. Buttons map to
   `agents.reply` (`once`, `always` when offered, `reject`). Correlate by
   `request_id`; disable after replying; show a toast on success/failure.
4. **Live behaviour**: append without stealing scroll unless "follow" is on;
   show a "new events" pill when scrolled up and events arrive.
5. **Empty/error states**: no output yet, no events (session without a
   structured feed), no runs; RPC failure shows a retry.
6. Do **not** attempt to render ANSI/terminal output: events are structured.

## Definition of done

- `cd gpui && cargo check && cargo test && cargo fmt --check && cargo clippy -- -D warnings`.
- A blocked run can be answered from the banner and the task resumes.
- Scrolling up while events stream does not jump; the "new events" pill
  scrolls back to live when clicked.
- Unknown event kinds are ignored gracefully.
