agent = "opencode"
provider = "opencode-go"
model = "deepseek-v4.1-flash"
worktree = false
permission = "allow"
needs = "gpui/02-transport:succeeded"
---

You are implementing **step 03 of 11** of the favetto GPUI desktop client.

Read first:
- `docs/design/gpui-client.md` (§1 product shape, §2 laws, §3 visual system).
- `gpui/README.md` and the existing `gpui/src/{app,theme,state}.rs`.

## Goal

Turn the placeholder shell into the real navigation frame: sidebar, top bar,
status bar, routing, command palette and keyboard model.

## Work

1. **Sidebar**: Inbox, Tasks, Catalog, Workflows, Schedules, Settings — icon +
   label, active state, awaiting-input count badge on Inbox (from state).
   Order follows the design doc (Serial Position Effect).
2. **Top bar**: active repo name, connection pill (`● Connected`,
   `◌ Reconnecting (n)`, `● Unauthorized`), `⌘K` hint, settings shortcut.
3. **Routes**: an enum + `ClientState.active_route`; each view is an entity.
   Provide real empty states (no "TODO" text) for routes not built yet.
4. **Command palette** (`⌘K` / `Ctrl+K`): fuzzy search over commands (navigate,
   open settings, reconnect, refresh). ↑/↓/Enter/Esc, visible focus, recent
   commands first. Use `fuzzy-matcher`.
5. **Keyboard model**: `g i`, `g t`, `g c`, `g w`, `g s` jumps; `Esc` closes
   overlays/back; `?` opens a shortcut sheet. Actions + `key_context`, not raw
   key-string matching.
6. **Responsive layout**: three regions on wide windows; list + pushed detail
   on narrow windows via `container_query`.
7. **Focus**: every interactive element shows a visible focus ring; Tab order
   follows the visual order.
8. Unit/UI tests where practical: palette filtering, route jumps, palette
   selection.

## Definition of done

- `cd gpui && cargo check && cargo test && cargo fmt --check && cargo clippy -- -D warnings`.
- Every route is reachable by keyboard from launch; the palette opens/closes
  with `⌘K`/`Esc`; the active route is visibly distinct.
- No placeholder "TODO" screens remain, only intentional empty states.
