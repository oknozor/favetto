# Panic safety and output bounding

favetto is a long-running daemon: a reachable panic in the request loop can drop
every attached TUI session at once. This page records the audit of the daemon's
production panic paths (issue #207), the policy that keeps the list from
growing, and the regression tests that guard it.

Two properties are in scope:

1. **No reachable panic on client input.** Malformed or hostile RPC params must
   become a typed `INVALID_PARAMS`/`INTERNAL` error, never a panic.
2. **Bounded persisted output.** Every finished run's `task.output` blob is
   capped by `[executor].max_output_bytes` (default 262144 bytes) regardless of
   the envelope shape an agent produces.

## Policy: scoped clippy restriction lints

The daemon crate denies the panic-prone restriction lints in its **non-test**
build, at both crate roots (`crates/favetto/src/lib.rs` and
`crates/favetto/src/main.rs`):

```rust
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented
    )
)]
```

`cfg_attr(not(test))` means the crate's inline `#[cfg(test)]` modules and
dedicated `*_tests.rs` files keep using `unwrap()`/`expect()` freely — they are
compiled with `cfg(test)` and are not restricted. The production build (what the
binary and integration tests link) is what is enforced.

Run the scoped gate with:

```bash
cargo clippy --locked -p favetto --all-targets -- -D warnings
```

A site that is provably infallible stays but carries a targeted, reasoned
`allow`, for example:

```rust
#[allow(
    clippy::expect_used,
    reason = "the default FavettoConfig configures only built-in agents, which always construct"
)]
```

## Inventory

Sites are grouped by decision. The scan strips `#[cfg(test)]` items, so the
"~600 occurrences" reported by a naive grep are almost entirely test code.

### Fixed: reachable production panics

| Site | Kind | Change |
|------|------|--------|
| `src/template.rs` `render` | `unwrap` on `template[i..].chars().next()` | Rewritten with `let … else { break }` (the loop guard already made it infallible) |
| `src/executor.rs` `build_task_output` | `expect` on `envelope.as_object_mut()` | Replaced with `if let Some(env) …` |
| `src/server.rs` `validate_create_dag` | `expect("key seeded")` on the user DAG's indegree map | Replaced with `if let Some(degree) …` |
| `src/server.rs` `create_workflow` | `expect`/index on `ids` for a user-supplied DAG | Replaced with `.get(..).ok_or_else(|| RpcError::Internal(..))?` |
| `src/server.rs` `inspect_workflow` | `tasks[0]` root-name fallback | Replaced with `tasks.first().map(..).unwrap_or_default()` |

### Justified with a reasoned `allow`

| Site | Reason |
|------|--------|
| `src/agents/registry.rs` `AgentRegistry::default` | The default `FavettoConfig` configures only built-in agents, which always construct |
| `src/docgen.rs` `docs_dir` | `CARGO_MANIFEST_DIR` is compile-time, so the repo root is always two ancestors up |

### Justified without an `allow`: guarded indexing, not `unwrap`/`expect`

These are slice/index operations already protected by a length, char-boundary, or
`windows(2)` guard. They are not flagged by the panic restriction lints
(`clippy::indexing_slicing` is intentionally not denied) and are recorded here so
the audit is complete:

| Site | Guard |
|------|-------|
| `src/agents/mod.rs` (session/task-id formatting) | UUID strings and fixed-width prefixes are 36/8 chars |
| `src/agents/opencode/observer.rs` (SSE/JSON parsing) | length and char-boundary checks |
| `src/executor.rs` (`branch_name`, git output parsing) | char-boundary guard / `windows(2)` |
| `src/webhooks.rs` (signature hex decode) | length check before slicing |
| `src/agents/pi.rs` (stream framing) | length / char-boundary guard |
| `src/agents/vibe/stream.rs` (stream framing) | length / char-boundary guard |
| `favetto-core/src/wire.rs` (`src[0..4]`) | guarded by `src.len() < 4` (different crate) |

### Not a panic site

`encode` (`src/server.rs`) intentionally falls back to `serde_json::Value::Null`
via `unwrap_or`, not `unwrap`.

## Output bounding

`build_task_output` (`src/executor.rs`) applies `max_output_bytes` to every run:

- raw transcript is capped head+tail by `truncate_text`,
- the parsed `result` is capped (or reduced to a `preview`),
- the result envelope is normalized and its `artifacts`/`findings`/`outputs`
  fields are bounded by `bound_envelope`.

Retention is handled separately: `db::prune` clears `tasks.output` **before**
deleting rows, so pruning cannot leave an oversized or half-deleted blob behind.
The existing `prune` test asserts `outputs_cleared == 1` and `got.output.is_none()`.

## Guard tests

| Test | Proves |
|------|--------|
| `server_tests::dispatch_rejects_malformed_params` | Every param-taking `dispatch` arm returns typed `INVALID_PARAMS` for a wrong-typed/missing required field |
| `server_tests::handle_request_rejects_malformed_special_params` | The `handle_request`-only arms (`agents.attach`, `agents.close`) reject malformed session params before touching the session manager |
| `server_tests::parse_params_never_panics_on_hostile_values` | A depth-bounded `proptest` over arbitrary JSON: no representative params type panics at the deserialization boundary |
| `executor_tests::build_task_output_bounds_every_shape` | The persisted blob stays within a small multiple of `max_output_bytes` for raw-text, structured-envelope, non-envelope, and unknown nested shapes — and survives a DB round-trip bounded |

## Follow-ups (out of scope here)

- A nightly `cargo fuzz` smoke job for the wire decoder and param parser.
- An optional DB-size (or output-bytes) Prometheus gauge to observe retention in
  production.
