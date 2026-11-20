agent = "opencode"
provider = "opencode-go"
model = "deepseek-v4.1-flash"
worktree = false
permission = "allow"
needs = "gpui/06-task-detail:succeeded"
---

You are implementing **step 07 of 11** of the favetto GPUI desktop client.

Read first:
- `docs/design/gpui-client.md` (Catalog; Choice Overload, Postel's Law).
- `gpui/README.md`, `gpui/src/{state,transport}.rs`.
- `crates/favetto-wire/src/rpc/messages.rs` (`CatalogEntry`, `CatalogAddParams`,
  `StartOneshotParams`, `TasksStartParams`).

## Goal

Browse the task catalog and launch a definition with its declared variables.

## Work

1. Load `catalog.list` and keep it fresh on `catalog.updated`.
2. **Grouping**: by repo namespace (`repo:<name>/…`), then folder; a search field
   filters names and prompts (fuzzy). Collapsible groups, remembered per session.
3. **Preview**: `catalog.get` renders the raw Markdown source in a read-only
   monospace/preview pane (no full Markdown renderer required; keep it legible).
4. **Launch sheet**: shows the task's agent/provider/model, `cwd`, schedule and
   declared `[[vars]]`. Required vars are validated inline; multiline values use
   a text area. `Cmd/Ctrl+Enter` launches, `Esc` cancels.
5. **Launch**: `tasks.start` with `{ name, input }`; on success close the sheet,
   toast, and navigate to the new task's detail. On failure keep the sheet open
   with the server error attached to the form.
6. **One-shot**: a secondary "New one-shot session" action from the toolbar
   using `tasks.start_oneshot` (agent/cwd picker); open the resulting session.
7. Empty state explains that tasks are Markdown files under the repo's
   `.favetto/tasks/`.

## Definition of done

- `cd gpui && cargo check && cargo test && cargo fmt --check && cargo clippy -- -D warnings`.
- Launching a catalog task with required variables is blocked until they are
  filled, and succeeds against a live daemon afterwards.
- A server error (e.g. unknown agent) is shown inline, not as a crash/log line.

## Commit

- After the definition of done is satisfied, create a git commit for this task before reporting completion.
- Review `git status` and the diff; stage only changes made for this task. Do not include pre-existing or unrelated changes, and never amend an existing commit.
- If task changes overlap pre-existing edits and cannot be cleanly isolated, stop and ask before committing.
- Use a concise commit message, e.g. `feat(gpui): build task catalog`.
