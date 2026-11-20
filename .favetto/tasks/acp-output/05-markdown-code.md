agent = "opencode"
provider = "opencode-go"
model = "deepseek-v4.1-flash"
worktree = false
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

- Work in the current checkout; preserve unrelated changes.
- Do not add raw HTML or active content support.
- Do not commit.

## Definition of done

- Assistant prose renders as Markdown; code blocks are language-aware and
  highlighted where supported.
- Relevant tests, formatting, and lint pass; report exact commands and results.
