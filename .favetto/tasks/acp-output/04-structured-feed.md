agent = "opencode"
provider = "opencode-go"
model = "deepseek-v4.1-flash"
worktree = true
permission = "allow"
needs = "acp-output/03-event-model:succeeded"
---

You are implementing step 2 of the ACP TUI output workflow: render ACP events
as a structured, readable feed rather than flattening them into ad-hoc terminal
text.

## Read first

- `crates/favetto-tui/src/tui/app/mod.rs` and `crates/favetto-tui/src/tui/ui.rs`
- `crates/favetto-tui/src/tui/term.rs` and related app state/tests
- `crates/favetto-wire/src/agent_state.rs`
- `web/src/components/agent_feed.rs` for interaction ideas only; do not port web
  code or alter the web experience.

## Shared pull-request branch

This task enriches the existing `feat/acp-tui-output` branch created by step 01;
step 07 will open the single pull request containing all workflow commits.

- Require `[executor] parallel = true` and `[executor] worktree = true`; if the
  task is not running in its own clean Favetto worktree, stop before editing.
- Fetch `origin/feat/acp-tui-output` and reset only this task's clean, generated
  worktree branch to that remote branch before making changes. Never reset the
  base checkout or another worktree.
- After implementation and checks pass, create a conventional commit (for
  example, `feat(tui): render structured ACP feed`) and push with
  `git push origin HEAD:refs/heads/feat/acp-tui-output`. Do not force-push.
- Do not open a separate pull request. Report the commit hash, branch, files
  changed, and checks for the next step.

## Goal

Give the active ACP session a structured TUI feed with clear message, thinking,
and tool-call hierarchy, while leaving PTY-backed sessions unchanged.

## Work

1. Add an ordered, bounded feed model for the active ACP session. Group adjacent
   text/thinking deltas appropriately and update tool cards in place by ID as
   their status and details arrive. Preserve event order and avoid duplicate
   cards for interleaved updates.
2. Render assistant messages as prose, user messages distinctly, and thoughts
   as a visually muted, collapsible “Thinking” section. Show a streaming marker
   only while the trailing thought is still being updated. Do not expose
   reasoning more prominently than the final response.
3. Render tool calls as compact rows/cards with an icon selected from ACP tool
   kind when known, a readable name/title, and a textual status. Unknown kinds
   and statuses must get a neutral fallback; color must not be the only status
   signal.
4. Make tool details foldable by keyboard. Collapsed rows show a useful summary;
   expanded rows show available input, output/content, locations, and terminal
   or diff content when the normalized event contains it. Missing details should
   simply be omitted. Keep the current selection/scroll behavior usable while
   events stream.
5. Keep the current PTY/terminal display path for non-ACP sessions. ACP-authored
   message text must be rendered as text, not interpreted as terminal control
   input.
6. Add focused state/UI tests for event coalescing, tool updates by ID,
   expand/collapse, unknown values, and preserving PTY behavior.

## Constraints

- Work only in this task's generated worktree; preserve unrelated changes.
- Keep the implementation within the favetto TUI and its tests.

## Definition of done

- ACP output is a structured feed with foldable tool detail and clear status;
  PTY sessions retain their existing rendering.
- Relevant tests and formatting pass, and the commit is pushed to
  `feat/acp-tui-output`; report exact commands and results.
