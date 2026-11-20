agent = "opencode"
provider = "opencode-go"
model = "deepseek-v4.1-flash"
worktree = false
permission = "allow"
needs = "acp-output/06-safe-rendering:succeeded"
---

You are implementing the final verification step of the ACP TUI output
workflow. Do not add features.

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
4. Do not start ACP tasks or invoke tools that alter or launch user tasks. Do not
   commit.
5. Finish with a concise report of files changed, commands and results, manual
   checks not performed, and known limitations. Do not claim a check passed if
   it was not run.

## Definition of done

- Relevant checks pass, or any remaining failure is documented with its exact
  output and whether it predates this change.
- The final report confirms that no task was started and no commit was created.
