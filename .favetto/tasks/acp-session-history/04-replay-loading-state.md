agent = "opencode"
provider = "opencode-go"
model = "deepseek-v4.1-flash"
worktree = false
permission = "allow"
needs = "acp-session-history/03-load-session:succeeded"
---

You are implementing **step 04 of 6** of Favetto's ACP session-history workflow.

Read first:
- `crates/favetto/src/agents/acp.rs` — notification normalization and transcript accumulation.
- `crates/favetto/src/agents/mod.rs` — event feed, attach snapshot, and session status.
- `crates/favetto-core/src/agent_state.rs` and `crates/favetto-wire/src/model.rs` — normalized event and session state types.
- `crates/favetto/src/event_bus.rs` — live push behavior.

## Goal

Make replayed ACP history safe to attach to: represent loading explicitly, apply replay updates in order, and report a session ready only after `session/load` succeeds.

## Work

1. Add explicit loading/ready/failed state to the session lifecycle and expose it in the session snapshot without breaking older wire clients.
2. Register the Favetto-local session before issuing the load request so ACP replay notifications have an owner.
3. Buffer or otherwise gate replay notifications so a failed load cannot leak a partial transcript as a successful restore. On success, apply the full replay in order and then publish readiness; on failure, surface the error and release buffered data safely.
4. Ensure attach/reconnect snapshots and event sequence cursors are consistent with the replayed state. Avoid duplicate text when the same client attaches during loading or reconnects after readiness.
5. Keep ordinary live prompt streaming behavior unchanged.
6. Add deterministic tests where the fake ACP agent sends multiple history updates before its load response, delays the response, and fails after emitting updates.

## Constraints

- Do not persist a second transcript copy as a substitute for ACP replay.
- Keep UI rendering changes for the next step; this step defines and tests the server/runtime readiness contract.
- Do not discard existing live-session events or alter task completion semantics.

## Definition of done

- Successful load exposes the complete replay before the session is marked ready.
- Delayed and failed loads have deterministic, observable states and do not produce a false successful restore.
- Tests cover event order, attach during loading, and no duplicate replay after reconnect.

## Commit

- After the definition of done is satisfied, create a git commit for this task.
- Review `git status` and the diff; stage only changes made for this task. Do not include pre-existing or unrelated changes, and never amend an existing commit.
- If task changes overlap pre-existing edits and cannot be cleanly isolated, stop and ask before committing.
- Use a concise commit message, e.g. `fix(acp): order replay before session readiness`.
