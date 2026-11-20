agent = "opencode"
provider = "opencode-go"
model = "deepseek-v4.1-flash"
worktree = true
---

You are a read-only specialist audit in the ACP TUI output workflow.

## Assignment

Scope: `{{ input.scope }}`

Focus: {{ input.focus }}

## Work

1. Inspect the relevant source and tests. Verify claims from source rather than
   assuming all ACP agents populate optional fields.
2. Report concrete file/type/function references, what data is already
   available, what is discarded, compatibility risks, and focused test cases.
3. Do not edit files or commit. Keep the response concise and useful to the
   implementation tasks. Unknowns should be called out explicitly.

This task is intentionally independent of the other audit scopes. It runs
read-only in its own Favetto worktree and may run in parallel with the other audit
scopes; the implementation waits for all audits to finish. The workflow requires
`[executor] parallel = true` and `[executor] worktree = true`.

## Definition of done

- The response contains findings only for the assigned scope, with concrete
  repository references and actionable recommendations.
- No repository files were changed.
