agent = "opencode"
provider = "opencode-go"
model = "deepseek-v4.1-flash"
worktree = false
permission = "allow"
needs = "gpui/10-polish:succeeded"
---

You are implementing **step 11 of 11** of the favetto GPUI desktop client.

Read first:
- `docs/design/gpui-client.md`, `gpui/README.md`, and the whole `gpui/src/` tree.

## Goal

Verify the client end to end and leave a durable record. Do not add features;
fix only regressions and build/test breakage.

## Work

1. Run in `gpui/`:
   - `cargo fmt --check`
   - `cargo clippy --all-targets -- -D warnings`
   - `cargo test`
   - `cargo build` (debug)
2. Run the app against the live daemon on this machine
   (`http://127.0.0.1:7878`, token from `~/.local/share/favetto/token`).
   Exercise **read-only** flows and one safe action:
   - connect, list tasks, open a task, switch tabs, open the catalog preview;
   - open Settings, run "Test connection";
   - do **not** start, cancel or retry any task.
3. Confirm the app degrades cleanly: wrong token → Unauthorized; daemon stopped →
   Reconnecting; daemon restarted → recovered.
4. Write `gpui/VERIFICATION.md`: commands run, results, manual checklist,
   known limitations, and any follow-up work (with file references).
5. If a check fails, fix it and re-run the full list. Do not declare success
   without a clean run.

## Definition of done

- All checks above pass.
- `gpui/VERIFICATION.md` exists and is accurate.
- No unrelated files were modified.

## Commit

- After the definition of done is satisfied, create a git commit for this task before reporting completion.
- Review `git status` and the diff; stage only changes made for this task. Do not include pre-existing or unrelated changes, and never amend an existing commit.
- If task changes overlap pre-existing edits and cannot be cleanly isolated, stop and ask before committing.
- Use a concise commit message, e.g. `test(gpui): verify desktop client`.
