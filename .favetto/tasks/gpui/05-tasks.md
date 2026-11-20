agent = "opencode"
provider = "opencode-go"
model = "deepseek-v4.1-flash"
worktree = false
permission = "allow"
needs = "gpui/04-inbox:succeeded"
---

You are implementing **step 05 of 11** of the favetto GPUI desktop client.

Read first:
- `docs/design/gpui-client.md` (Tasks surface; Hick's Law, Fitts's Law, Doherty).
- `gpui/README.md`, `gpui/src/{state,transport,app}.rs`.

## Goal

Build the Tasks list: every run, searchable and filterable, with safe actions.

## Work

1. Load `tasks.list` (limit 500) and keep it live from `task.updated` pushes.
2. **Filters**: All / Running / Awaiting input / Failed / Succeeded, as a single
   segmented control (one dimension — no filter maze). Counts per segment.
3. **Search**: fuzzy over task name, agent, repo; `/` focuses the field;
   `Esc` clears. Preserve the filter when searching.
4. **Rows**: status icon + label, name, repo badge, agent, attempt, duration,
   cost when known. Awaiting rows use the accent treatment from the inbox.
5. **Actions**: `Enter` opens detail; `c` cancels (confirmation dialog naming
   the task); `r` retries a terminal task (confirmation). Both are optimistic:
   update locally immediately, reconcile on RPC error with a toast and rollback.
6. **Selection**: ↑/↓ moves, Home/End, PageUp/PageDown; selection survives list
   updates when the id is still present.
7. **States**: loading skeleton, empty (filtered vs truly empty), error with
   retry. Keep the previous list visible during a refresh (no flicker).

## Definition of done

- `cd gpui && cargo check && cargo test && cargo fmt --check && cargo clippy -- -D warnings`.
- Filter counts match the rows; cancel/retry work against a live daemon and
  roll back visibly when the RPC fails.
- No list row panics on unknown status/agent values.
