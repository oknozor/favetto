# Prompt templates and the task graph

## Prompt templates

Prompt bodies and `spawn_file` paths are rendered before use. `{{ dotted.path }}`
placeholders resolve against a JSON context; strings render raw, objects and
arrays render as compact JSON, and missing paths render empty. Use double braces
to leave single braces alone (for example, code blocks).

| Placeholder | Resolves to |
|-------------|-------------|
| `{{ task.id }}` | The task run's UUID. |
| `{{ task.name }}` | The catalog identity (relative path, no `.md`). |
| `{{ input.<name> }}` | A collected `[[vars]]` value, or input produced by `spawn`/webhooks. |
| `{{ prev.* }}` | The result of the `needs` predecessor (also available as `input._prev`). |

For `needs = "<task>:all_finished"`, `prev` is the fan-in aggregate:
`root`, `target`, `count`, `succeeded`, `failed`, `cancelled`, `tasks`.

### The result envelope

A finished run carries an additive `envelope` so a dependent can read a
predecessor's result without parsing a transcript. Read it with
`{{ prev.output.envelope.summary }}`.

| Field | Type | Description |
|-------|------|-------------|
| `summary` | string | Always present; a one-line result summary. |
| `artifacts` | array | Things the run produced; may be empty. |
| `findings` | array | Observations the successor should know. |
| `outputs` | object | Structured values keyed by name; defaults to `{}`. |
| `continuation` | object or null | Optional hint for what to do next. |

Prefer durable artifacts (`{"kind":"commit","ref":"<branch>"}` or
`{"kind":"pr","ref":"#123"}`) over worktree-local file paths, which are removed
unless the agent committed or `keep_worktree` is set.

## Dependencies: `needs`

`needs` names a predecessor task plus how to react to its outcome. The suffix
comes after a colon:

| Value | Starts |
|-------|--------|
| `<task>:finished` (alias `<task>:terminal`, or a bare `<task>`) | Once per finished predecessor run, on **either** outcome. |
| `<task>:succeeded` | Once per successful predecessor run. |
| `<task>:failed` | Once per failed predecessor run. |
| `<task>:all_finished` | Once per workflow root, after **every** `<task>` run of that root is terminal; `prev` is the aggregate. |

Each start attaches the predecessor result as `prev.*`. `needs` is mutually
exclusive with `schedule`.

## Fan-out: `spawn`

When a run succeeds, `spawn` reads the JSON file at `spawn_file` (rendered as a
template) as an **array** and enqueues one `spawn` task per element, passing that
element as its `input`. Write `[]` to enqueue nothing.

- `spawn_file` is relative to the task's working directory (or absolute).
- The file must contain the JSON array and nothing else — no prose, no code
  fences.
- `spawn_new_root = true` makes each child its own workflow root instead of a
  descendant, which lets a pipeline restart itself so the child's own
  `:all_finished` fan-in fires freshly.

## Schedules

`schedule` is a cron expression that registers the task as a recurring
`catalog:<name>` schedule. Editing or removing the key re-syncs the schedule on
reload. Manually created schedules are left alone. `schedule` is mutually
exclusive with `needs`.

## Failure classification

A failed run carries `error` (human-readable) and `failure` (machine-readable):

| Field | Description |
|-------|-------------|
| `kind` | One of `agent`, `infrastructure`, `timeout`, `invalid_input`, `dependency`, `cancelled`, `blocked`, `unknown`. |
| `message` | The same detail as `error`. |
| `retryable` | Whether an unattended retry could help. `true` by default for `infrastructure` and `timeout`; `false` otherwise, including `agent`, `invalid_input`, and `blocked`. |

`blocked` is a failure kind, not a status: a blocked run is terminal and is never
retried automatically.
