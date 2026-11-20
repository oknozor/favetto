agent = "opencode"
provider = "opencode-go"
model = "deepseek-v4.1-flash"
worktree = true
permission = "allow"
needs = "acp-output/02-audit:all_finished"
---

You are implementing step 1 of the ACP TUI output workflow: preserve structured
ACP tool-call information through favetto's normalized event model.

## Read first

- Audit results: `{{ prev.tasks }}`
- `crates/favetto/src/agents/acp.rs`
- `crates/favetto-wire/src/agent_state.rs`
- All consumers and tests of `AgentStateEvent` (including the web feed).
- The ACP v1 schema version and documentation used by the current adapter.

## Shared pull-request branch

This is the first implementation step. It and steps 04–06 must enrich the same
branch, `feat/acp-tui-output`, which step 01 seeded from `origin/main` (excluding
unpublished local commits such as concurrent GPUI work). The final step opens
one pull request from that branch to `main`.

- Require `[executor] parallel = true` and `[executor] worktree = true`; if the
  task is not running in its own clean Favetto worktree, stop before editing.
- Fetch `origin/feat/acp-tui-output` and reset only this task's clean, generated
  worktree branch to that remote branch before making changes. Never reset the
  base checkout or another worktree.
- After the implementation and checks pass, create a conventional commit, for
  example `feat(tui): preserve structured ACP tool events`, then push with
  `git push origin HEAD:refs/heads/feat/acp-tui-output`. Do not force-push.
- Do not open a separate pull request. Report the commit hash, shared branch,
  files changed, and exact checks so the next step can continue the same PR.

## Goal

Make the tool-call lifecycle and available detail usable by clients without
breaking existing event consumers or assuming optional ACP fields are present.

## Work

1. Trace how tool start/update/finish events are currently normalized. Preserve
   the tool call ID, kind, status, name/title, input, output/content, and
   locations when the ACP v1 message provides them. Keep unknown or absent
   values representable and retain current behavior for agents that send only a
   title/name and status.
2. Keep updates correlated by tool-call ID. Ensure partial updates do not erase
   previously known input or metadata, and final success/failure/cancellation
   statuses are represented correctly when available in the current schema.
3. Update affected consumers additively, preserving the existing web feed and
   daemon behavior. Do not broaden this change into ACP v2 migration.
4. Add focused unit tests for sparse updates, streamed/final content where
   supported, multiple interleaved tool IDs, and agents that omit optional
   fields. Follow existing schema and event conventions.

## Constraints

- Work only in this task's generated worktree; preserve unrelated changes.
- Keep changes limited to ACP event normalization, its consumers required for
  compilation, and tests.

## Definition of done

- `AgentStateEvent` retains the detail needed for a useful tool card while
  remaining tolerant of missing fields.
- Relevant tests and formatting pass, and the commit is pushed to
  `feat/acp-tui-output`; report exact commands and results.
