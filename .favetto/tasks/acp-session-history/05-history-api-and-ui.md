agent = "opencode"
provider = "opencode-go"
model = "deepseek-v4.1-flash"
worktree = false
permission = "allow"
needs = "acp-session-history/04-replay-loading-state:succeeded"
---

You are implementing **step 05 of 6** of Favetto's ACP session-history workflow.

Read first:
- `crates/favetto/src/server.rs` and `crates/favetto-wire/src/rpc/` — current agents RPC surface.
- `crates/favetto-tui/src/tui/mod.rs` and `crates/favetto-tui/src/tui/app/mod.rs` — current `Ctrl+O` session picker and attach flow.
- `web/src/client.rs`, `web/src/components/agent_feed.rs`, and the relevant web views — current web agent-session flow.
- `gpui/src/transport.rs`, `gpui/src/state.rs`, `gpui/src/settings.rs`, and `gpui/src/views/settings.rs` — GPUI RPC wrappers, shared client state, and current Agents surface.
- `docs/guide/agents.md` and `docs/guide/tui.md` — user-facing lifecycle documentation.

## Goal

Let users find and load their previously created ACP sessions, while keeping Favetto IDs separate from provider ACP IDs and clearly communicating unsupported or failed restores.

## Work

1. Add an additive, typed RPC contract for listing Favetto's persisted session records and requesting a load by Favetto-local session ID. Use the repository's existing naming and error conventions; do not expose the external ACP ID as the client routing key.
2. Include enough display metadata to identify a session (agent, title when available, working directory, timestamps, lifecycle/load state). Keep current live-session listing and attach behavior compatible.
3. Update the TUI `Ctrl+O` picker to include persisted sessions and distinguish active, ended, loading, failed, and load-unsupported states. Selecting a loadable past session invokes the new RPC and shows progress/errors rather than opening a blank new session.
4. Wire both the web and GPUI clients to the same list/load contract. In GPUI, expose persisted sessions in the existing Agents/settings surface (or a better-fitting existing session surface), with loading, ready, unsupported, and failed states; selecting a past session must request load by Favetto-local ID and show progress/errors. Do not add a separate protocol or duplicate session store.
5. Document the difference between reattaching to an in-process session and loading a provider-persisted session. State that importing arbitrary provider sessions via ACP `session/list` remains out of scope.
6. Add wire/server/client tests for old-client compatibility, history listing, load selection, and clear handling of unsupported/load-failed sessions. Include GPUI state/transport coverage for list, load-by-local-ID, loading/replay success, and load failure.

## Constraints

- Do not implement ACP `session/list` or import sessions created outside Favetto.
- Never silently start a fresh ACP session when a user selects a past one.
- Keep transcript ownership with the ACP agent. Do not treat bounded/prunable `task.output` as a full transcript.
- Keep changes focused; preserve current `agents.attach` semantics for live or in-process retained sessions.

## Definition of done

- A persisted Favetto-created session is discoverable and can be loaded from the TUI using its local ID.
- Existing RPC clients and live-session flows remain compatible; the web surface uses the same contract where applicable.
- GPUI can discover and load a persisted session through the same contract, clearly distinguishing active, ended, loading, failed, and unsupported states without silently starting a new session.
- Focused TUI, web, GPUI, RPC, and server tests pass; docs explain capability limitations.

## Commit

- After the definition of done is satisfied, create a git commit for this task.
- Review `git status` and the diff; stage only changes made for this task. Do not include pre-existing or unrelated changes, and never amend an existing commit.
- If task changes overlap pre-existing edits and cannot be cleanly isolated, stop and ask before committing.
- Use a concise commit message, e.g. `feat(agents): expose past ACP sessions`.
