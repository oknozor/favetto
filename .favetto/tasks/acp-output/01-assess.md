agent = "opencode"
provider = "opencode-go"
model = "deepseek-v4.1-flash"
worktree = false
permission = "allow"
spawn = "acp-output/02-audit"
spawn_file = ".favetto/plans/acp-output/{{ task.id }}/audits.json"
---

You are the entry point for the ACP TUI output improvement workflow. Starting
`acp-output/01-assess` launches the complete workflow; do not run any of its
child tasks directly.

## Goal

Prepare a small read-only audit fan-out before implementation. The implementation
must improve the favetto TUI's structured ACP output with tool/thinking icons,
foldable tool detail, Markdown and code highlighting, and safe rendering.

## Work

1. Read the existing ACP adapter and normalized event types, the TUI event
   handling/rendering path, the TUI Markdown and JSON renderers, and the web feed
   only as a compatibility reference. Start with:
   - `crates/favetto/src/agents/acp.rs`
   - `crates/favetto-wire/src/agent_state.rs`
   - `crates/favetto-tui/src/tui/app/mod.rs`
   - `crates/favetto-tui/src/tui/ui.rs`
   - `crates/favetto-tui/src/tui/markdown.rs`
   - `crates/favetto-tui/src/tui/json.rs`
   - `crates/favetto-tui/src/tui/term.rs`
   - `web/src/components/agent_feed.rs`
2. Confirm the actual ACP version and fields used by this repository. Treat ACP
   protocol documentation as guidance, not as proof that a given agent reports
   optional tool input, output, or content.
3. Write `.favetto/plans/acp-output/{{ task.id }}/audits.json` as a JSON array
   containing exactly these three independent audit requests:

   - `acp-adapter`: trace ACP updates through normalization; identify available
     fields, patch/lifecycle behavior, affected consumers, and tests.
   - `tui-feed`: trace the active TUI render path; propose the smallest
     structured feed model and interactions for messages, thoughts, and
     foldable tool calls without changing PTY-session behavior.
   - `markdown-safety`: inspect existing Markdown/JSON rendering and terminal
     handling; recommend Markdown/code highlighting and ACP content safety
     measures, including tests.

   Each array element must be an object with `scope` and `focus` string fields.
   Create the parent directory. The file must contain JSON only, with no prose
   or code fences.
4. Do not edit product source files. Do not commit.

## Definition of done

- The audit manifest contains exactly the three scopes above and is valid JSON.
- The task succeeds so the fan-out can start.
