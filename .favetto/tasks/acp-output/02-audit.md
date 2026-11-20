agent = "opencode"
provider = "opencode-go"
model = "deepseek-v4.1-flash"
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

This task is intentionally independent of the other audit scopes. It may run in
parallel with them; the implementation waits for all audits to finish.

## Definition of done

- The response contains findings only for the assigned scope, with concrete
  repository references and actionable recommendations.
- No repository files were changed.
