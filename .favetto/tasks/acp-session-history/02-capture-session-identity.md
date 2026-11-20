agent = "opencode"
provider = "opencode-go"
model = "deepseek-v4.1-flash"
worktree = false
permission = "allow"
needs = "acp-session-history/01-session-store:succeeded"
---

You are implementing **step 02 of 6** of Favetto's ACP session-history workflow.

Read first:
- `crates/favetto/src/agents/acp.rs` and `crates/favetto/src/agents/mod.rs` — ACP startup and manager lifecycle.
- `crates/favetto/src/executor.rs` — task/run creation and finalization.
- `crates/favetto/src/server.rs` — interactive and one-shot session startup.
- `crates/favetto/src/db.rs` — session store from step 01 and task-run updates.

## Goal

Populate the durable session record for every Favetto-created ACP session, and persist the provider's ACP session ID as soon as it becomes available—not only after a task completes.

## Work

1. At session launch, persist the stable Favetto-local ID, configured agent name, working directory, optional task/run linkage, and initial lifecycle status.
2. When the ACP `session/new` response identifies the provider session, update the record immediately. Also persist title changes when ACP reports them.
3. Update the associated `task_runs` row as soon as its ACP session ID is known; retain the existing finalization path as an idempotent backstop.
4. Cover all current launch paths: interactive `agents.start`, one-shot interactive tasks, and headless catalog task runs. Mark closed/failed state without deleting the durable record.
5. Define and test failure behavior when launch or metadata persistence fails. A failed metadata write must not silently advertise a session as resumable.

## Constraints

- Work only in session lifecycle/persistence code and focused tests; do not send `session/load` yet.
- Keep the Favetto-local ID used by `agents.attach` and other Favetto RPCs separate from the ACP ID sent to the agent.
- Do not store credentials or the transcript.

## Definition of done

- A session ID is durable before a headless task is finalized.
- Tests prove metadata is recorded for each launch path and remains after the live manager entry is removed.
- Existing task/run finalization semantics remain unchanged.

## Commit

- After the definition of done is satisfied, create a git commit for this task.
- Review `git status` and the diff; stage only changes made for this task. Do not include pre-existing or unrelated changes, and never amend an existing commit.
- If task changes overlap pre-existing edits and cannot be cleanly isolated, stop and ask before committing.
- Use a concise commit message, e.g. `feat(acp): persist session ids on creation`.
