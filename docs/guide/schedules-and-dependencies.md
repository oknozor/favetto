# Schedules and dependencies

Tasks can run on a timer or in response to another task. Both are declared in
the task header; no external scheduler is needed.

## Run on a schedule

Add a cron expression to make a task recurring:

```md
schedule = "0 0 8 * * *"
---
Summarise new issues opened in the last day.
```

The expression has six fields — second, minute, hour, day of month, month, day
of week — so `0 0 8 * * *` is every day at 08:00. The daemon registers the task
as a `catalog:<name>` schedule on load and re-syncs it when the header changes or
is removed. Schedules created by hand through the Ctrl+P menu or RPC are left
alone.

`schedule` and `needs` are mutually exclusive: a task is either timer-driven or
dependency-driven.

## Run after another task

`needs = "<task>:finished"` starts this task when the named task finishes. The
predecessor's result is attached as `input._prev`, reachable as
<span v-pre>`{{ prev.* }}`</span>:

```md
agent = "opencode"
needs = "triage:finished"
---
A triage run just finished. Its output was:

{{ prev.output }}

Write a plan based on it.
```

`prev.output` is the predecessor's stored output object (`agent`, `output`,
`output_bytes`, `truncated`, `result`, …), not a bare string. Stored output is
capped (default 256 KiB, `[executor].max_output_bytes`); longer runs are
truncated head+tail and have `truncated = true`. When the agent produced no
structured result of its own, the default parser's `{"text": raw}` duplicate is
dropped and `result` is `null` — read `output` for the raw text, or `result` for
structured JSON.

A `needs` successor is an unattended run: no `[[vars]]` form is shown, so
`required` variables must be provided by the predecessor or the run fails fast.

### Branch on the outcome

`needs = "<task>:finished"` fires on **either** terminal outcome, so a controller
must inspect `prev.success` to branch. When the branch is known ahead of time,
use an outcome condition instead:

| `needs` value | Starts when the predecessor… |
|---------------|--------------------------------|
| `<task>` / `<task>:finished` / `<task>:terminal` | finishes, successfully **or** not |
| `<task>:succeeded` | succeeds |
| `<task>:failed` | fails |

All forms behave the same otherwise: the successor is an unattended run that
still receives the predecessor result as <span v-pre>`{{ prev.* }}`</span>, and
`:terminal` is just an explicit spelling of `:finished`. This lets one step fan
out to an `implement` task on success and a `diagnose` task on failure without a
controller:

```md
agent = "opencode"
needs = "research:failed"
---
The research step failed with:

{{ prev.output }}

Diagnose the failure and propose a fix.
```

## Chain a pipeline with `spawn`

`spawn` + `spawn_file` fan a task out into children. When a task succeeds, the
file at `spawn_file` is read as a JSON array and one `spawn`-named task is
enqueued per element, with that element as its `input`:

```md
agent = "opencode"
spawn = "plan"
spawn_file = ".favetto/{{ task.id }}/manifest.json"
---
Read the issue queue and write one entry per issue to
<span v-pre>`.favetto/{{ task.id }}/manifest.json`</span> as a JSON array of objects with the
fields `number` and `title`. If there is nothing to do, write `[]`.
```

```md
agent = "opencode"
---
Plan issue {{ input.number }}: {{ input.title }}.
```

An empty array spawns nothing, so a task can decline to continue. This makes a
triage → plan → implement pipeline fully declarative:

1. **triage** writes a manifest array and `spawn`s **plan** once per element;
2. each **plan** writes a handoff file and `spawn`s **implement**;
3. **implement** opens the pull request.

## Wait for every instance (fan-in)

A fan-out has no built-in join: `needs = "<task>:finished"` starts its successor
**once per finished instance**. When a step must run **once** after *all* the
instances descended from the same run are done, use the root-scoped fan-in
`needs = "<task>:all_finished"`:

```md
agent = "opencode"
needs = "implement:all_finished"
---
Every implementation spawned by this workflow run has finished.

Results (JSON): {{ prev.tasks }}

Summarise the pull requests and flag the failures.
```

The join starts exactly once per **workflow root** — the task run that started
the fan-out, tracked through `spawn` lineage and persisted across daemon
restarts. Two concurrent fan-outs from the same catalog task stay independent:
each resolves its own join.

The barrier counts every **terminal** state (success, failure, cancelled), and an
empty fan-out (`[]`) still resolves it. The aggregated results are attached as
`input._prev`, reachable as <span v-pre>`{{ prev.* }}`</span>:

```json
{
  "kind": "all_finished",
  "root": { "task_id": "…", "name": "triage" },
  "target": "implement",
  "count": 5,
  "succeeded": 4,
  "failed": 1,
  "cancelled": 0,
  "tasks": [
    {
      "task_id": "…",
      "name": "implement",
      "status": "succeeded",
      "success": true,
      "session_id": "…",
      "input": { "issue_id": 123 },
      "output": { "…": "…" }
    }
  ]
}
```

<span v-pre>`{{ prev.tasks }}`</span> renders the whole array, so the successor can
review every child's output and react to `prev.failed`. A fan-in whose target is
never spawned in a root never fires; only the "spawned zero" case is covered.

::: info Bounded payload
Because the rendered prompt is passed to the agent as one command-line argument
and the OS caps a single argument at 128 KiB, large per-run outputs are bounded
before they are embedded in `_prev`: each `output` is truncated head+tail (the
tail keeps the run's final summary), a bulky `output.result` duplicate is
dropped, and the whole `tasks` array is capped. Bounded entries keep their keys
and set `"truncated": true`. The same bound applies to a single
`":finished"` dependency's `prev.output`.
:::

## Restart a pipeline

A fan-in successor runs **once per workflow root**, so a successor that re-spawns
its own fan-out is normally deduped: its join key already fired for that root.
Set `spawn_new_root = true` to break the cycle — each `spawn` child starts as its
own workflow root, so its fan-in gets a fresh barrier:

```md
agent = "opencode"
needs = "implement:all_finished"
spawn = "triage"
spawn_file = ".favetto/merge/{{ task.id }}/manifest.json"
spawn_new_root = true
---
If issues remain, write `[{}]` to the manifest to triage another round;
otherwise write `[]` to stop.
```

The `triage` child is its own root, so when its `implement` children finish the
`:all_finished` fan-in fires again and starts a new successor instead of being
silently dropped. Write `[]` in the handoff to end the loop. Without
`spawn_new_root`, the child inherits the spawner's root and the successor runs at
most once per pipeline.

::: tip Inspect the graph
Press `w` in the TUI to render the catalog's `needs`, outcome-condition
(`needs:succeeded` / `needs:failed`), `spawn`, and fan-in `join` edges as a
box-drawing graph (also persisted as Graphviz DOT at `<data_dir>/workflow.dot`).
:::

![favetto workflow graph](/screenshots/workflow-graph.png)

*The `w` workflow overlay: `needs`, outcome, `spawn`, and `join` edges between
tasks.*

See the [task file format reference](../reference/tasks) for the exact header
keys, and [Events](../reference/events) for `task_finished` and the other
lifecycle events.
