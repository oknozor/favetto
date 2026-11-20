agent = "opencode"
provider = "opencode-go"
model = "deepseek-v4.1-flash"
worktree = false
permission = "allow"
---

You are implementing **step 01 of 6** of Favetto's ACP session-history workflow.

Read first:
- `docs/guide/agents.md` — current ACP lifecycle and limitation.
- `docs/design/acp-migration.md` — protocol and implementation decisions.
- `crates/favetto/src/db.rs` — SQLite schema and migration conventions.
- `crates/favetto/src/agents/mod.rs` — in-memory session identity and lifecycle.
- `crates/favetto-wire/src/model.rs` and `crates/favetto-wire/src/rpc/` — public session types.

## Goal

Add durable **session metadata** so Favetto can find sessions it created after a daemon restart. ACP agents remain the source of truth for conversation history; do not add transcript persistence in this step.

## Work

1. Add an additive SQLite migration and typed persistence operations for ACP session records. Store at least:
   - a stable Favetto-local session ID (distinct from the agent's ACP session ID);
   - agent configuration name and nullable external ACP session ID;
   - working directory, optional task/run IDs, optional title;
   - creation/update timestamps and a lifecycle status sufficient to distinguish loading, ready, ended, and failed records.
2. Keep external IDs scoped by agent; do not assume IDs from different ACP servers are globally unique.
3. Preserve a record when an in-memory session closes. Do not store agent environment variables, credentials, or transcript content in this table.
4. Follow existing SQLx/migration conventions and make the operations usable by both interactive sessions and task-backed sessions.
5. Add database tests for migration, insert/update, list ordering, and reopening the database with records intact.

## Constraints

- Work only in the database/session model layers and their tests. Do not add ACP loading, RPCs, or UI in this step.
- Keep schema changes additive and preserve existing task/run data.
- Do not persist secrets or duplicate task output as a transcript.

## Definition of done

- Session metadata survives closing and reopening the SQLite database.
- Tests cover nullable external IDs, stable local IDs, and agent-scoped external IDs.
- Focused database tests and formatting pass.

## Commit

- After the definition of done is satisfied, create a git commit for this task.
- Review `git status` and the diff; stage only changes made for this task. Do not include pre-existing or unrelated changes, and never amend an existing commit.
- If task changes overlap pre-existing edits and cannot be cleanly isolated, stop and ask before committing.
- Use a concise commit message, e.g. `feat(acp): persist session metadata`.
