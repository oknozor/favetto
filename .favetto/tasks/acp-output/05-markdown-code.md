agent = "opencode"
provider = "opencode-go"
model = "deepseek-v4.1-flash"
worktree = true
permission = "allow"
needs = "acp-output/04-structured-feed:succeeded"
---

You are implementing step 3 of the ACP TUI output workflow: Markdown rendering
and code syntax highlighting for the structured feed.

## Read first

- `crates/favetto-tui/src/tui/markdown.rs`
- `crates/favetto-tui/src/tui/json.rs`
- The new structured feed renderer and its tests.
- `crates/favetto-tui/Cargo.toml` before considering a dependency.

## Shared pull-request branch

This task enriches the existing `feat/acp-tui-output` branch created by step 01;
step 07 will open the single pull request containing all workflow commits.

- Require `[executor] parallel = true` and `[executor] worktree = true`; if the
  task is not running in its own clean Favetto worktree, stop before editing.
- Fetch `origin/feat/acp-tui-output` and reset only this task's clean, generated
  worktree branch to that remote branch before making changes. Never reset the
  base checkout or another worktree.
- After implementation and checks pass, create a conventional commit (for
  example, `feat(tui): render ACP markdown and code`) and push with
  `git push origin HEAD:refs/heads/feat/acp-tui-output`. Do not force-push.
- Do not open a separate pull request. Report the commit hash, branch, files
  changed, and checks for the next step.

## Goal

Render ACP text blocks as readable Markdown and fenced code as language-aware,
syntax-highlighted code, using existing TUI conventions where practical.

## Work

1. Extend or adapt the existing Markdown renderer for assistant text in the ACP
   feed. Support common CommonMark constructs (headings, lists, emphasis,
   blockquotes, links, inline code, fenced blocks) and preserve readable
   fallbacks for unsupported constructs.
2. Retain the fenced code language label and apply syntax highlighting for
   common languages. Reuse the JSON highlighter where appropriate. Prefer an
   existing dependency or a small maintainable implementation; justify any new
   dependency and keep unsupported languages readable without highlighting.
3. Render tool JSON input/output with the existing JSON renderer, and render
   content types such as text and diff distinctly where available. Do not treat
   tool titles, paths, commands, or other plain-text protocol fields as
   Markdown.
4. Add tests for Markdown blocks, fenced language selection, JSON, unknown
   languages, and the plain-text fallback.

## Constraints

- Work only in this task's generated worktree; preserve unrelated changes.
- Do not add raw HTML or active content support.

## Definition of done

- Assistant prose renders as Markdown; code blocks are language-aware and
  highlighted where supported.
- Relevant tests, formatting, and lint pass, and the commit is pushed to
  `feat/acp-tui-output`; report exact commands and results.
