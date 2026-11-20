agent = "opencode"
provider = "opencode-go"
model = "deepseek-v4.1-flash"
worktree = false
permission = "allow"
needs = "acp-session-history/05-history-api-and-ui:succeeded"
---

You are implementing **step 06 of 6** of Favetto's ACP session-history workflow.

Read first:
- `AGENT.md` — required workspace checks and commit conventions.
- `docs/guide/agents.md`, `docs/guide/tui.md`, and all changed session-history code.

## Goal

Verify the full past-session loading path and leave an accurate record. Do not add features; fix only regressions and verification failures caused by this workflow.

## Work

1. Run focused tests for session metadata migrations, early ACP ID persistence, capability negotiation, history replay ordering, RPC compatibility, and UI state.
2. Run the repository gates from `AGENT.md`: formatting, Clippy, workspace tests, dependency check, generated-reference drift check, and docs check. Use the documented commands and record any environment limitation precisely.
3. Use the deterministic fake ACP server for end-to-end coverage. Verify that it can create a session, exit, be loaded by external ID after daemon/store recreation, replay history before readiness, and accept a follow-up prompt.
4. Verify negative cases: no `loadSession` capability, agent load error, failed initialization, and daemon restart during an active task. Confirm loading a past session does not re-run the task or mark an interrupted run successful.
5. Do not require credentials or live third-party ACP agents. Do not change retry policy or add session discovery/import.
6. Update the agent guide with verified behavior and limitations. Report commands/results and any remaining issues in the task response; do not create a verification artifact unless the existing project convention requires one.

## Definition of done

- The full supported check suite passes, or every blocked check is reported with its exact environmental cause.
- End-to-end tests prove restored history is visible only after a successful load and that local and ACP IDs are not confused.
- No unrelated files are modified.

## Commit

- After the definition of done is satisfied, create a git commit for this task.
- Review `git status` and the diff; stage only changes made for this task. Do not include pre-existing or unrelated changes, and never amend an existing commit.
- If task changes overlap pre-existing edits and cannot be cleanly isolated, stop and ask before committing.
- Use a concise commit message, e.g. `test(acp): verify past session loading`.
