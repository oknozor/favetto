# Durable workflow orchestration

Design and implementation plan for turning favetto's existing task graph into a
durable, inspectable execution substrate that an external controller (human or
LLM) can safely drive.

Status: proposed
Scope: shared types (`crates/favetto-core`), daemon (`crates/favetto`), TUI
(`crates/favetto-tui`), docs.
Non-goal: replacing the embedded terminal, the `needs`/`spawn` model, or the
`all_finished` fan-in, and — importantly — **adding an agent loop to the daemon**.
Per the architecture, favetto orchestrates; all reasoning stays in external CLIs.

---

## 1. Motivation

favetto already schedules and runs declarative tasks with workflow lineage,
fan-out/fan-in, dedup, parallel execution, worktree isolation, and a durable
event log. What it does **not** yet have is the layer that makes that machinery
safe and legible to an automated controller:

- a task has **no execution-attempt history** — `Task` conflates the logical work
  item with one run;
- a daemon crash collapses a run to `failed` with no record of what happened;
- there is **no retry policy**, so infrastructure faults are indistinguishable
  from genuine task failure;
- the only workflow query is the **catalog-derived** graph, not the running one;
- failures are free text, so a controller cannot reason about them.

This document specifies the smallest set of changes that closes those gaps
without disturbing the existing workflow semantics.

---

## 2. Current state (verified against the tree)

| Claim | Evidence | Status |
|---|---|---|
| `needs` / `spawn` / `spawn_file` / `spawn_new_root`, lineage | `favetto-core/src/tasks.rs`, `executor.rs` (`Lineage`) | present |
| Fan-out / fan-in, `_prev`, per-directory serialization | `executor.rs` (`spawn_from_manifest`, `maybe_start_join`, `DirLocks`) | present |
| Parallelism + git-worktree isolation | `executor.rs` (`make_plan`), `config.rs` (`ExecutorSettings`) | present |
| Deduplication, durable events + replay cursor | `db.rs` (`dedupe_key UNIQUE`, `events`), `event_bus.rs` | present |
| Structured `Task.input` / `Task.output` | `model.rs`; envelope built by `executor::build_task_output` | present (agent-centric) |
| Catalog-derived workflow graph (`workflow.get`) | `favetto-core/src/workflow.rs` (`build_graph`, `build_dot`) | present |
| Startup recovery | `db::fail_interrupted_tasks`, called in `daemon.rs` | present but terminal-only |
| Per-attempt execution record | — | **missing** |
| Retry policy | — | **missing** |
| Runtime (as opposed to catalog) workflow query / control | — | **missing** |
| Typed failure | `Task.error: Option<String>` only | **missing** |
| `needs` conditions beyond `finished` / `all_finished` | `tasks.rs` (`needs_parts`, `validate_needs`) | **missing** |

Two details to keep in mind throughout:

- `executor::is_terminal` currently matches `Succeeded | Failed` only. It
  **excludes `Cancelled`**, so any change to status handling must revisit it.
- `needs = "<task>:finished"` intentionally fires on **success or failure**; the
  `TaskFinished` payload carries `success`. That behaviour must be preserved.

---

## 3. Design constraints

1. **The daemon never implements an agent loop.** External controllers (including
   any LLM supervisor) talk to the daemon through the remote API; the daemon stays
   deterministic.
2. **`favetto-core` has no HTTP/DB/LLM concerns.** New domain types live there;
   persistence and RPC handling stay in `crates/favetto`.
3. **Wire changes are additive.** New `Task`/config fields need `#[serde(default)]`
   and legacy-decode tests.
4. **Generated reference must not drift.** New config fields, RPC methods, or
   event kinds require regenerating `docs/reference/*` and
   `docs/public/favetto-schema.json` via `cargo run -p favetto -- __doc`, with a
   clean `git diff`.
5. **One focused change at a time**, Conventional Commits, inline tests, and the
   full `AGENT.md` gate set before push.

---

## 4. Scope decisions

**Adopt**

- Typed failure semantics (`FailureKind` + `retryable`).
- Execution attempts (`task_runs`) as a separate record.
- An idempotent startup reconciler with an explicit stale-run policy.
- Infrastructure-only retry, plus a manual retry RPC.
- A runtime workflow query (`workflow.inspect`) and root-scoped control.
- `needs` condition variants (`:succeeded` / `:failed` / `:terminal`).
- A richer `TaskFinished` payload.

**Adapt**

- **`Blocked` is a failure kind, not a `TaskStatus`.** A new non-terminal status
  would ripple through the dispatcher, `is_terminal`, retention, the TUI, and the
  wire decode. A typed terminal failure with `retryable = false` gives a
  controller the same signal for far less churn. Revisit only if "blocked but
  resumable" becomes a real requirement.
- **`workflow.create` is split.** Read-only `workflow.inspect` first (small,
  unlocks controllers). Dynamic per-instance DAGs need a new dependency field and
  evaluation path — a separate, larger feature. `workflow.spawn` is a thin public
  wrapper over the existing `enqueue_with_lineage`, not a new engine.
- **`workflow.pause`/`resume` are deferred.** They need a persistent per-root
  dispatch flag and a dispatcher check.

**Drop**

- Event sourcing: SQLite remains authoritative; the event log stays a durable
  observation stream and resume cursor.
- An in-daemon LLM supervisor; see §8.

---

## 5. Domain model changes (`favetto-core`)

### 5.1 Typed failure

Add to `model.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// The agent ran and reported failure (non-zero exit, rejected work, …).
    Agent,
    /// The daemon could not start or supervise the run (spawn/PTY/daemon fault).
    Infrastructure,
    /// The run exceeded its deadline.
    Timeout,
    /// Input was invalid (missing required vars, unparseable manifest, …).
    InvalidInput,
    /// A dependency could not be satisfied.
    Dependency,
    /// Cancelled by a user or controller.
    Cancelled,
    /// Cannot proceed until something external changes (credentials, human
    /// decision, unavailable agent).
    Blocked,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub kind: FailureKind,
    pub message: String,
    /// Whether an unattended retry could plausibly help.
    pub retryable: bool,
}
```

`Task` gains, defaulted:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub failure: Option<Failure>,
#[serde(default)]
pub attempt: u32,
```

`error` remains for display and compatibility; `failure` is the machine-readable
view.

### 5.2 Result envelope

Keep `Task.output: serde_json::Value` and the existing
`{agent, session_id, session_title, output_bytes, truncated, output, result}`
shape. Layer a documented convention on top, produced from the agent's structured
`result` when present:

```json
{
  "summary": "Implemented GitHub webhook signature verification.",
  "artifacts": [
    { "kind": "source", "path": "src/github.rs" },
    { "kind": "commit", "ref": "favetto/implement-github-abc12345" }
  ],
  "findings": [],
  "outputs": { "tests_passed": true },
  "continuation": null
}
```

Conventions, not a mandatory schema: arbitrary JSON from existing tasks keeps
working, and `build_task_output` synthesises a `summary` from the tail of the raw
output when the agent offers nothing structured.

> Artifact durability: a run's worktree is deleted unless `keep_worktree` is set
> or the agent committed/pushed. Artifact paths must therefore be repo-relative,
> and a `branch`/`commit`/`pr` artifact is more useful than a worktree-local file
> path. Document this in the result-envelope section of the task reference.

### 5.3 Execution attempts

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Pending,
    Running,
    AwaitingInput,
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
    TimedOut,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRun {
    pub id: Uuid,
    pub task_id: Uuid,
    pub attempt: u32,
    pub status: RunStatus,
    pub agent: Option<String>,
    pub session_id: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    pub failure: Option<Failure>,
}
```

`Task` stays the logical item; per-run blobs (session, output) remain on `Task`
for compatibility. The run row is a history/metrics/reconciliation record.

---

## 6. Execution substrate

### 6.1 Per-attempt runs

`db::claim_task` becomes the point that also creates a run row atomically:
`status -> running` on the task and the same transition on a new run with
`attempt = task.attempt`. `run_one` finalises the run on completion, and
`fail_task` finalises it as `failed` (or `interrupted` for reconciliation).

### 6.2 Retention

`db::prune` must also delete `task_runs` whose task row is removed, and the
startup `VACUUM` accounting should include them. Add the run table and an index
on `(task_id, attempt)`; keep migrations idempotent and additive like the
existing `ALTER TABLE ... ADD COLUMN` block.

### 6.3 Reconciliation

Replace the unconditional `fail_interrupted_tasks` with an idempotent
`reconcile()` run at startup (and safe to run repeatedly):

1. Any active run whose process is gone becomes `interrupted`.
2. The owning task's fate follows `[executor.stale_run]`:
   `fail` (default, preserving today's behaviour) or `retry`.
3. Pending tasks whose catalog definition has vanished are failed with
   `FailureKind::InvalidInput` (they currently fail lazily in the dispatcher).
4. Worktree orphans are already handled by `prune_worktrees`; keep that.

The reconciler must not touch terminal tasks, and running it twice must be a
no-op.

### 6.4 Retry

Add `[executor.retry]`:

```toml
[executor.retry]
max_attempts = 3
backoff = "exponential"      # or "fixed"
initial_ms = 5000
max_ms = 300000
retry_on = ["infrastructure", "timeout"]
```

Only `FailureKind` values in `retry_on` are retried automatically; `agent` and
`invalid_input` never are. A retry re-enqueues the task at `attempt + 1` after the
backoff and records a new run. `tasks.retry` exposes the same path manually for a
terminal task.

---

## 7. Workflow control plane

### 7.1 `workflow.inspect`

A read-only, compact runtime view by `root_id`, distinct from the catalog
`workflow.get`:

```json
{
  "root_id": "…",
  "root_task": "implement_issue",
  "state": "running",
  "tasks": [
    { "id": "…", "name": "research", "status": "succeeded", "attempt": 1,
      "summary": "…" },
    { "id": "…", "name": "implement_issue", "status": "running", "attempt": 2 }
  ],
  "ready": [],
  "running": ["…"],
  "failed": [],
  "blocked": []
}
```

It must not embed `output` blobs; per-task detail stays behind `tasks.get`. Add a
`db::list_root_tasks` helper using the existing `idx_tasks_root_id` index and the
`root_id = ? OR id = ?` rule already used by `list_tasks_in_root`.

### 7.2 Root-scoped control

- `workflow.cancel { root_id }` cancels every non-terminal task in the root and
  emits `TaskCancelled` per task; no further `needs`/join work fires for it.
- `workflow.retry { task_id }` is the manual retry from §6.4.

Keep single-task `tasks.cancel` unchanged.

### 7.3 `needs` conditions

Extend `NeedsKind` (and `validate_needs`, `needs_parts`, the workflow edge kind,
and the TUI workflow view):

- bare / `:finished` — start on either terminal outcome (unchanged);
- `:succeeded` — start only when the predecessor succeeded;
- `:failed` — start only when the predecessor failed;
- `:terminal` — explicit alias for `:finished`.

`start_dependents` filters on the `success` field already present in the
`TaskFinished` payload. This enables `research -> implement` on success and
`research -> diagnose` on failure without a controller.

### 7.4 Dynamic instance-level workflows

`workflow.create` / `workflow.spawn` let an external controller build runtime
task graphs that reference catalog definitions and carry **per-instance**
dependencies, evaluated alongside the name-based `needs` path. No second
scheduler: the existing dispatcher, lineage, dedupe, and `TaskFinished` listener
are reused.

**Representation — a dedicated `task_dependencies` table.**

```sql
task_dependencies(task_id, depends_on, kind, created_at)
  PRIMARY KEY (task_id, depends_on)
```

`kind` is reserved and currently always `'finished'`: a dependent starts once its
predecessor reaches a terminal state (`succeeded` / `failed` / `cancelled`).
Cancellation counts as terminal because it does not emit `TaskFinished`, so an
otherwise-cancelled predecessor must not wedge a dynamic dependent.

**Readiness gate.** Dynamic nodes are inserted `Pending` up front (durable and
inspectable) and gated inside `db::next_pending_tasks` with a `NOT EXISTS
(... non-terminal predecessor ...)` subquery, so the existing dispatcher and
dependency listener are unchanged. Readiness is re-derived from SQLite on every
poll, so a restart needs no replay. A missing (pruned) predecessor does not
block. Catalog `needs` dependents carry no dependency rows, so their dispatch is
untouched. `_prev` injection is intentionally not done for dynamic dependencies;
their inputs are supplied at creation.

**RPCs.**

```
workflow.create {
  idempotency_key: String,
  root_id?: Uuid,
  tasks: [{ key, name, input?, depends_on?: [key] }]
} -> { root_id, tasks: [{ key, id, name }] }
```

The whole request is validated before any insert (non-empty, unique keys, known
catalog names, in-request dependency keys, acyclic, known root). Per-node dedupe
keys `workflow:{idempotency_key}:{key}` make re-submission return the same ids
without creating a second run. The first task is the root unless `root_id` is
given; every other node becomes its child.

```
workflow.spawn {
  name, input?, root_id?, depends_on?: [task_id], dedupe_key?
} -> Task
```

is a thin wrapper over the existing enqueue/lineage path that records dependency
rows; with a `dedupe_key` it returns the canonical stored row.

`workflow.inspect` reports a `Pending` node as `blocked` when any of its
`task_dependencies` predecessors is still non-terminal, in addition to the
catalog-`needs` rule.

**Non-goals.** No new `TaskStatus` or `Task` wire field; no outcome-conditional
dynamic dependencies, cross-root dependencies, or `all_finished` joins over
dynamic edges; no TUI change; no cascade-cancel.

### 7.5 Richer `TaskFinished`

Extend the payload from `{name, task_id, success}` to include `status`,
`attempt`, `retryable`, and a bounded `summary`. Existing consumers keep reading
`name`/`task_id`/`success`.

---

## 8. Supervisor as an external controller

The supervisor is **not** daemon code. It is a program (or an external agent
driven by a script) that:

1. observes via `workflow.inspect` (plus `events.subscribe` for a live cursor);
2. decides among a **strict action vocabulary**:
   `inspect`, `spawn`, `cancel`, `retry`, `wait`, `request_input`, `complete`,
   `escalate`;
3. submits mutations through the workflow RPCs.

Favetto validates and executes; the supervisor only expresses intent. `wait` is a
first-class decision so the supervisor does not spawn work while existing work is
in flight. The contract (observation schema, decision schema, action-to-method
mapping) is documented so any controller — a custom binary, a web UI, a human, or
another agent — can drive the same daemon.

This preserves the architectural boundary in `docs/reference/architecture.md`
("the orchestrator never implements its own agent loop") while still enabling
LLM-directed workflows.

---

## 9. Delivery phases

Each task below is independently reviewable. Sizes: S ≈ half day, M ≈ 1–2 days,
L ≈ 3–5 days.

### Phase 0 — domain vocabulary

**P0.1 Typed failure** · S–M — `model.rs`, `db.rs`, `executor.rs`; additive
migration and legacy-decode test; classify non-zero exit vs spawn fault vs
missing catalog vs timeout.

**P0.2 Result envelope** · S–M — `executor::build_task_output`; parse from the
agent `result` when present, synthesise a summary otherwise; preserve existing
`_prev` bounding (`bounded_prev_value`).

**P0.3 Richer `TaskFinished`** · S — `executor.rs`; additive payload fields.

### Phase 1 — durable execution

**P1.1 `task_runs` model + persistence** · M — `model.rs`, `db.rs`; schema,
CRUD, index, prune integration.

**P1.2 Attempt-per-run in the executor** · M — `executor.rs`; `claim_task`
creates the run, `run_one`/`fail_task` finalise it; `Task.attempt` advances.

**P1.3 Idempotent reconciler** · M — new module (or extend `executor.rs`),
`daemon.rs`, `config.rs` (`stale_run` policy); default preserves today's
behaviour.

**P1.4 Infrastructure-only retry** · M–L — `config.rs`, `executor.rs`,
`server.rs`/`rpc.rs` (`tasks.retry`); depends on P0.1 and P1.1–P1.3.

### Phase 2 — workflow control plane

**P2.1 `workflow.inspect`** · M — `db.rs`, `server.rs`, `rpc.rs`; independent,
best first API win.

**P2.2 Root-scoped cancel/retry** · M — depends on P2.1; retry on P1.4.

**P2.3 `needs` conditions** · S–M — `tasks.rs`, `workflow.rs`, `executor.rs`,
TUI workflow view; independent.

**P2.4 Dynamic instance-level workflows** · L — `task_dependencies` plus
`workflow.create` / `workflow.spawn`; depends on P2.1, independent of retry.

### Phase 3 — supervisor

**P3.1 Supervisor contract** · S — docs: observation/decision schema and
action-to-method mapping.

**P3.2 Reference external supervisor** · M — one worked example outside the
daemon; depends on P3.1 and P2.1/P2.2.

### Dependency graph

```text
P0.1 ─┬─► P0.2
      └─► P0.3
P1.1 ─► P1.2 ─► P1.3 ─► P1.4 ─┐
                              ├─► P2.2 ─► P3.2
P2.1 ─────────────────────────┘
P2.3 (independent)
P2.4 ◄─ P2.1
P3.1 ─► P3.2
```

**Smallest valuable milestone** ("durable + inspectable", no retry, no
supervisor): `P0.1, P0.3, P1.1, P1.2, P1.3, P2.1, P2.3`.

Suggested commit split (Conventional Commits, per `AGENT.md`):
`feat(core): add typed task failure`, `feat(executor): record task run attempts`,
`feat(executor): reconcile interrupted runs at startup`,
`feat(executor): retry retryable failures`, `feat(api): add workflow.inspect`,
`feat(api): add root-scoped workflow control`,
`feat(tasks): add needs outcome conditions`, and a docs commit for the supervisor
contract. Use `feat!` only if a field becomes required.

---

## 10. Testing

- **Unit**: failure classification; run creation/finalisation; reconciler
  idempotence (run twice, same state); backoff scheduling; `needs` filtering per
  outcome; `workflow.inspect` bucketing.
- **DB**: additive migration on a legacy database; `task_runs` pruning; root
  queries against `idx_tasks_root_id`.
- **RPC**: `workflow.inspect`, `workflow.cancel`, `tasks.retry` round-trips with
  the existing fake-agent/PTY fixtures.
- **Compat**: deserialize legacy `Task` payloads (missing `failure`/`attempt`) and
  legacy `TaskFinished` payloads unchanged; confirm `:finished` still fires on
  failure.
- **Docs**: `cargo run -p favetto -- __doc` diff clean and `docs:check` passes.
- Existing gates from `AGENT.md`: `cargo fmt`, `clippy -D warnings`, `nextest`.

---

## 11. Risks and mitigations

| Risk | Mitigation |
|---|---|
| Retrying a genuine coding failure burns budget and repeats the same prompt | Retry only `Infrastructure`/`Timeout` by default; agent failures require a manual retry or a corrective task |
| `Cancelled` is excluded from `is_terminal`, so retention already mis-handles it | Fix `is_terminal` while touching the status handling; assert in tests |
| A new `Task` field breaks older clients | `#[serde(default)]` + legacy-decode tests; keep `error` alongside `failure` |
| Event/API drift failing CI | Regenerate `__doc` in the same change as any config/RPC/event addition |
| Dynamic DAGs inventing a second scheduling path | Keep P2.4 behind a spike; reuse the name-based listener and dedupe conventions |
| Supervisor becoming a de-facto in-daemon subsystem | Keep it an external client; the daemon exposes only the RPC/control plane |
| Unbounded `task_runs` growth | Prune with the owning task; include in `VACUUM` accounting |

---

## 12. Resolved decisions

The questions left open above were decided when the work was split into issues;
the answers are recorded here and mirrored in the tracking issue
(`oknozor/favetto#140`).

1. **Keep `workflow.get`; add `workflow.inspect`.** The two views answer
   different questions, and `workflow.get` is already on the wire, so it is not
   renamed or aliased. A `detail` level is unnecessary: per-task detail stays
   behind `tasks.get`. (Issue #142.)
2. **`Blocked` is a `FailureKind`, not a `TaskStatus`.** A new non-terminal
   status would ripple through the dispatcher, `is_terminal`, retention, the TUI,
   and the wire decode; a terminal `FailureKind::Blocked` with
   `retryable = false` gives a controller the same signal for far less churn.
   Revisit only if a resumable blocked state is ever required. (Issue #141.)
3. **The stale-run policy is executor-wide for the MVP.** `[executor.stale_run]`
   defaults to `fail`, preserving today's behaviour; a per-task override is
   deferred until a use case appears. (Issue #148.)
4. **Dynamic workflows use a dedicated `task_dependencies` table.** Widening
   `Task` with `depends_on` would put the graph on the wire and still need a
   readiness query; a table is restart-safe, queryable, and keeps `Task` stable.
   The readiness gate lives in `db::next_pending_tasks`, and
   `workflow.create` / `workflow.spawn` dedupe re-submissions with per-node
   dedupe keys. (Issue #151.)
