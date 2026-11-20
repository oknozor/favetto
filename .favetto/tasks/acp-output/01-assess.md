agent = "opencode"
provider = "opencode-go"
model = "deepseek-v4.1-flash"
worktree = true
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
foldable tool detail, Markdown and code highlighting, and safe rendering. The
implementation steps will add commits to one shared feature branch and the final
step will open one pull request; the feature branch is isolated from the ongoing
GPUI work.

## Worktree and branch setup

- This workflow requires `[executor] parallel = true` and `[executor] worktree = true`; task-level `worktree = true` is ignored when parallel execution is disabled. If either setting is off, stop before doing work and ask for it to be enabled.
- Use the Favetto-created worktree only. Before writing the audit handoff, confirm it is clean and fetch `origin/main`.
- Check whether `origin` already has `feat/acp-tui-output`. If it does not, seed it from `origin/main` by pushing `refs/remotes/origin/main` to `refs/heads/feat/acp-tui-output`. This deliberately excludes unpublished local commits (including concurrent GPUI work) from the ACP branch. If the branch already exists, reuse it only after confirming it belongs to this ACP-output workflow; otherwise stop and ask rather than overwrite it. Never force-push.
- Do not create the pull request yet. Steps 03–06 will push their implementation commits to this shared branch in dependency order; step 07 will open the single final pull request against `main`.

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
4. Do not edit product source files or create a commit. The only remote change in this step is seeding the shared feature branch as described above.

## Definition of done

- The audit manifest contains exactly the three scopes above and is valid JSON.
- The shared branch was seeded from the fetched `origin/main` base or safely reused after verifying it belongs to this workflow.
- The task succeeds so the fan-out can start.
