agent = "opencode"
provider = "opencode-go"
model = "deepseek-v4.1-flash"
worktree = true
permission = "allow"
needs = "acp-output/06-safe-rendering:succeeded"
---

You are implementing the final verification step of the ACP TUI output
workflow in its own worktree. Do not add features. This step opens the single PR
containing the commits from steps 03–06 on `feat/acp-tui-output`.

## Worktree and PR branch

- Require `[executor] parallel = true` and `[executor] worktree = true`; if the
  task is not running in its own clean Favetto worktree, stop before editing.
- Fetch `origin/feat/acp-tui-output` and reset only this task's clean, generated
  worktree branch to that remote branch before reviewing or testing. Never reset
  the base checkout or another worktree.
- If fixes are needed, keep them in scope, commit them using Conventional
  Commits, and push with `git push origin HEAD:refs/heads/feat/acp-tui-output`.
  Do not force-push.
- After all checks pass, find an existing open PR with head
  `feat/acp-tui-output` and base `main`. Update it if it belongs to this workflow;
  if it belongs to unrelated work, stop and report the collision rather than
  modifying it. If none exists, create the PR. Use the title
  `feat(tui): improve structured ACP output`
  and a body summarizing the implementation, safety measures, and verification.
  Use the configured GitHub integration or authenticated `gh` CLI. Do not merge
  the PR. Finish with its URL.

## Read first

- `crates/favetto-wire/src/agent_state.rs` and its tests
- `crates/favetto/src/agents/acp.rs` and its tests
- `crates/favetto-tui/src/tui/` feed/rendering code and tests
- `web/src/components/agent_feed.rs` for compatibility regressions

## Work

1. Review the complete diff for scope, event lifecycle correctness, missing
   optional ACP fields, unknown-value fallbacks, and compatibility with current
   web and PTY clients.
2. Run formatting, clippy, and tests for every affected Rust package. Identify
   package names from the workspace manifests rather than guessing. Run broader
   workspace tests if practical.
3. Fix only regressions or test/build breakage caused by this workflow, then
   rerun all failed checks.
4. Do not start ACP tasks or invoke tools that alter or launch user tasks.
5. Finish with a concise report of files changed, commands and results, manual
   checks not performed, and known limitations. Do not claim a check passed if
   it was not run.

## Definition of done

- Relevant checks pass, or any remaining failure is documented with its exact
  output and whether it predates this change.
- All workflow commits are present on `feat/acp-tui-output` and the final PR is
  open against `main`; include the PR URL in the report.
- No ACP user task was started.
