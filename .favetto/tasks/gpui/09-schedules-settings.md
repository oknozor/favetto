agent = "opencode"
provider = "opencode-go"
model = "deepseek-v4.1-flash"
worktree = false
permission = "allow"
needs = "gpui/08-workflows:succeeded"
---

You are implementing **step 09 of 11** of the favetto GPUI desktop client.

Read first:
- `docs/design/gpui-client.md` (Schedules, Settings; Postel's Law, Tesler's Law).
- `gpui/README.md`, `gpui/src/{config,transport,state}.rs`.

## Goal

Complete the administrative surfaces: schedules and connection/agent settings.

## Work

1. **Schedules**: `schedules.list` in a table (cron, task, enabled, next run).
   Create/edit sheet: cron expression with inline validation and a plain-language
   hint, task picker from `catalog.list`, enabled toggle. Delete with a
   confirmation. `schedules.upsert` / `schedules.delete`; refresh after each.
2. **Settings → Connection**: daemon URL + token. Accept `host:port`,
   `http(s)://…`, or a full `/rpc` URL; show the normalized result before saving.
   "Test connection" runs `system.ping` and reports latency or the exact error.
   Persist to `~/.config/favetto/gpui.toml`; offer "Use defaults" (token file).
3. **Settings → Agents**: `agents.list` with availability, command and backend
   (`acp`), read-only.
4. **Settings → Appearance**: theme (system/dark/light), density
   (comfortable/compact); apply live and persist.
5. **Settings → About**: version, daemon version/cwd from `system.ping`, docs
   link. No telemetry.

## Definition of done

- `cd gpui && cargo check && cargo test && cargo fmt --check && cargo clippy -- -D warnings`.
- A bad URL keeps the old connection and shows a precise validation message.
- Creating a schedule makes it appear with a computed `next_run`; deleting asks
  first and removes it.
- Settings survive an app restart.

## Commit

- After the definition of done is satisfied, create a git commit for this task before reporting completion.
- Review `git status` and the diff; stage only changes made for this task. Do not include pre-existing or unrelated changes, and never amend an existing commit.
- If task changes overlap pre-existing edits and cannot be cleanly isolated, stop and ask before committing.
- Use a concise commit message, e.g. `feat(gpui): add schedules and settings`.
