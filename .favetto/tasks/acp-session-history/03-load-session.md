agent = "opencode"
provider = "opencode-go"
model = "deepseek-v4.1-flash"
worktree = false
permission = "allow"
needs = "acp-session-history/02-capture-session-identity:succeeded"
---

You are implementing **step 03 of 6** of Favetto's ACP session-history workflow.

Read first:
- `crates/favetto/src/agents/acp.rs` — the existing v1 initialize/new/prompt loop.
- `crates/favetto/src/agents/mod.rs` — manager launch and lookup APIs.
- `crates/favetto/Cargo.toml` — pinned `agent-client-protocol` version.
- The ACP migration design and agent guide for protocol conventions.

## Goal

Add a generic, capability-aware ACP load path for sessions already created by Favetto. Do not add vendor-specific resume commands or provider-wide session discovery.

## Work

1. Retain the relevant capabilities returned during ACP initialization, including whether the server supports `session/load`.
2. Add a load launch mode that starts the configured agent process with the stored working directory, initializes ACP, and sends `LoadSessionRequest` for the stored external ACP session ID instead of creating a new session.
3. Continue to use the stable Favetto-local ID for routing; bind the newly launched process to that local record while keeping the provider ACP ID as separate metadata.
4. Do not send a new prompt during load. Preserve the existing new-session behavior for ordinary launches.
5. If the agent does not advertise load support, return a clear unsupported error. Never silently fall back to `session/new` and present it as a restored conversation.
6. Add fake-agent tests for supported load, unsupported capability, and agent-reported load failure.

## Constraints

- Use the existing generic ACP client and pinned protocol crate; do not add per-agent adapters or shell resume arguments.
- Limit this step to sessions Favetto already knows about. `session/list` discovery/import is out of scope.
- Do not change task retry/reconciliation policy; loading a conversation must not rerun the task.

## Definition of done

- A known external ACP session can be loaded through the generic client when supported.
- Unsupported and failed loads are distinguishable from successful restores.
- New sessions and normal prompt turns continue to work as before.

## Commit

- After the definition of done is satisfied, create a git commit for this task.
- Review `git status` and the diff; stage only changes made for this task. Do not include pre-existing or unrelated changes, and never amend an existing commit.
- If task changes overlap pre-existing edits and cannot be cleanly isolated, stop and ask before committing.
- Use a concise commit message, e.g. `feat(acp): load persisted sessions`.
