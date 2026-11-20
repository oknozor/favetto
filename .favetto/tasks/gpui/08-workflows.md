agent = "opencode"
provider = "opencode-go"
model = "deepseek-v4.1-flash"
worktree = false
permission = "allow"
needs = "gpui/07-catalog:succeeded"
---

You are implementing **step 08 of 11** of the favetto GPUI desktop client.

Read first:
- `docs/design/gpui-client.md` (Workflows; Goal-Gradient, Chunking).
- `gpui/README.md`, `gpui/src/{state,transport}.rs`.
- `crates/favetto-wire/src/workflow.rs` and the `workflow.*` RPCs.

## Goal

Show one workflow root's instances and let the user act on it.

## Work

1. Entry point: from a task detail's "View workflow" (root id) and a Workflows
   list of roots seen in the current task list.
2. `workflow.inspect`: render the five buckets — ready, running, failed,
   blocked, terminal — as sections of rows with status, attempt, dependencies.
   Show `done / total` and a progress bar (Goal-Gradient; never fake totals).
3. `workflow.get`: show the catalog DAG source (DOT) in a monospace panel with a
   short legend. Do not embed Graphviz; a readable source view is enough.
4. Actions: cancel the root (confirmation naming the count), retry a failed
   instance, refresh. Optimistically mark pending and reconcile.
5. Live refresh on `task.updated` events for tasks in the root; keep selection.
6. Empty state: a task with no root shows an explanation, not a blank panel.

## Definition of done

- `cd gpui && cargo check && cargo test && cargo fmt --check && cargo clippy -- -D warnings`.
- A two-step workflow shows both instances and their dependency; cancelling the
  root updates every non-terminal row.
- Retrying a failed instance re-enqueues it and the row reflects the new attempt.
