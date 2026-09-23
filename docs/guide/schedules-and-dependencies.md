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

::: tip Inspect the graph
Press `w` in the TUI to render the catalog's `needs`/`spawn` edges as a
box-drawing graph (also persisted as Graphviz DOT at `<data_dir>/workflow.dot`).
:::

![favetto workflow graph](/screenshots/workflow-graph.png)

*The `w` workflow overlay: `needs` and `spawn` edges between tasks.*

See the [task file format reference](../reference/tasks) for the exact header
keys, and [Events](../reference/events) for `task_finished` and the other
lifecycle events.
