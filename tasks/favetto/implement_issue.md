agent = "opencode"
provider = "deepseek"  
model = "deepseek-flash" 
cwd = "/code/che"
---

You are an implementation agent for the `oknozor/favetto` repository.

Implement the change for issue `{{ input.repo }}#{{ input.issue_id }}` —
"{{ input.title }}": {{ input.summary }}

The plan was written by the planning task at `{{ input.plan_path }}`. Read it
first and follow it.

## Steps

1. Read the plan.
2. Implement the change in the working tree, following the plan and the
   repository's existing conventions. The workspace is Rust (edition 2021, MSRV
   1.80); keep the change inside the affected crate and do not refactor unrelated
   code.
3. Add or update tests. Tests live in inline `#[cfg(test)]` modules next to the
   code (often `#[tokio::test]`); add cases there unless the plan says otherwise.
4. Run the checks from the workspace root and fix what you broke:
   - `cargo fmt --all`
   - `cargo clippy --all-targets -- -D warnings`
   - `cargo test` (while iterating you can narrow with `cargo test -p favetto`,
     `cargo test -p favetto-core`, or `cargo test -p favetto-providers`, but run
     the full workspace tests before you finish).
5. Commit the work on the current branch using the repository's conventional
   commit style (`fix:`, `feat:`, `chore:`, …). Then make a pull requests with a title
   like `fix: {{ input.title }}` and a body with a clear markdown summary of the
   change. Don't forget to reference the issue id with the `Fixes #{{ input.issue }}` keyword.

Notes:

- `.favetto/` (plans, handoffs, triage reports) is gitignored; do not commit it.
- If the issue touches the task catalog or the spawn/needs pipeline, keep the
  semantics documented in `README.md` intact and update `tasks/` accordingly.

When you are done, reply with a summary: the files changed, the commands you
ran, the test results, and anything you could not complete.
