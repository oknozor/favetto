agent = "opencode"
provider = "opencode-go"
model = "deepseek-v4.1-flash"
worktree = false
permission = "allow"
needs = "gpui/09-schedules-settings:succeeded"
---

You are implementing **step 10 of 11** of the favetto GPUI desktop client.

Read first:
- `docs/design/gpui-client.md` in full, especially §2 (the laws table) and the
  accessibility constraint.
- `gpui/README.md` and every view under `gpui/src/views/`.

## Goal

Turn a working client into a trustworthy one. No new features.

## Work

1. **Accessibility**: every interactive element has an accessible label; Tab
   order matches visual order; focus rings are never removed; status is never
   color-only; minimum target size 32 px (40 px for primary actions).
2. **States audit**: every view has loading, empty, error and unauthorized
   states. No `unwrap`/`expect` on network or persisted data. Errors are
   actionable and copyable.
3. **Reduced motion**: respect the OS setting for any animation; no essential
   information conveyed only by motion.
4. **Performance**: use `uniform_list`/virtualized rendering for long task and
   event lists; avoid cloning the full task list per frame; cap retained events.
5. **Consistency**: one spacing/type/color source (`theme.rs`); inventory and
   remove ad-hoc values.
6. **Keyboard-only pass**: navigate every surface, launch a task, answer a
   permission, cancel/retry, open settings — without a mouse. Fix gaps.
7. **UX review**: walk the ten design-review questions from the Laws of UX
   checklist; record findings and fixes in `gpui/README.md`.
8. Update `gpui/README.md` with screenshots or a short walkthrough and the
   known limitations list.

## Definition of done

- `cd gpui && cargo check && cargo test && cargo fmt --check && cargo clippy -- -D warnings`.
- Keyboard-only pass completed and documented.
- No view renders a bare error string or a blank panel on failure.

## Commit

- After the definition of done is satisfied, create a git commit for this task before reporting completion.
- Review `git status` and the diff; stage only changes made for this task. Do not include pre-existing or unrelated changes, and never amend an existing commit.
- If task changes overlap pre-existing edits and cannot be cleanly isolated, stop and ask before committing.
- Use a concise commit message, e.g. `chore(gpui): polish accessibility and UX`.
