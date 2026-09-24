# Task file format

A catalog task is a Markdown file under the tasks directory (default `tasks/`)
whose first section is a TOML header and whose body is the prompt. Everything
before the first `---` line is parsed as TOML; everything after it is the prompt:

```md
agent = "opencode"
provider = "jev"
model = "1.13"
cwd = "/code/che"
schedule = "0 8 * * * *"
needs = "another_task:finished"

[[vars]]
name = "issue"
prompt = "Which issue?"
required = true
---
You are an engineering agent. Implement {{ input.issue }}.
```

See the [catalog guide](../guide/catalog) for the workflow and the
[variables and prompts guide](../guide/variables-and-prompts) for runnable
examples.

## Header keys

| Key | Type | Required | Description |
|-----|------|----------|-------------|
| `agent` | string | no | Name of an `[agents.*]` entry. Overrides `[agent].default`. A task with neither fails to start. |
| `provider` | string | no | Model provider, substituted as `{provider}`. Ignored unless `model` is also set. |
| `model` | string | no | Model id, substituted as `{model}`. When set, the agent's `run_args` template is used instead of `headless_args`. |
| `cwd` | string | no | Working directory the agent runs in. Takes precedence over the agent's configured `cwd`; a per-run `input.cwd` wins over both. |
| `schedule` | string | no | A cron expression that makes this a recurring task. Mutually exclusive with `needs`. |
| `needs` | string | no | Dependency on another task: `<task>:finished` (or its alias `<task>:terminal`) auto-starts once per finished `<task>` run on **either** outcome, while `<task>:succeeded` / `<task>:failed` start only on that outcome; each attaches the result as <span v-pre>`{{ prev.* }}`</span>. `<task>:all_finished` is the root-scoped fan-in: auto-start once after **every** `<task>` run of this workflow root is terminal, with an aggregate <span v-pre>`{{ prev.tasks }}`</span>. Mutually exclusive with `schedule`. |
| `spawn` | string | no | Catalog task to enqueue from this task's handoff file when this task succeeds. Requires `spawn_file`. |
| `spawn_file` | string | no | Path of the JSON handoff file consumed by `spawn`. Rendered as a template at run time; read as a JSON array, one child per element. |
| `spawn_new_root` | bool | no | When true, each `spawn` child is enqueued as **its own workflow root** instead of a descendant of this task. Lets a pipeline restart itself so the child's own fan-in is not deduped against the root that already fired it. Requires `spawn`; default `false`. |
| `sign` | `"off"` \| `"ssh"` \| `"gpg"` | no | Per-task override of the `[git] signing` mode. Most specific level: task → `[agents.<name>.git]` → `[git]`. |
| `[[vars]]` | array of tables | no | Manual input variables; see below. |

An unknown or invalid key makes the whole file fail to parse, and the running
daemon keeps the previous definition of that task (a half-written edit never
drops a task).

## `[[vars]]`

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | yes | The `input` key. Must match `[a-zA-Z0-9_]+`, be unique within the file, and must not be `_prev`. |
| `prompt` | string | yes | The label/question shown in the TUI form. |
| `default` | string | no | Value pre-filled in the form. |
| `required` | bool | no | Block submission while empty (default `false`). Unattended runs fail when the value is absent. |
| `multiline` | bool | no | `Enter` inserts a newline and the form is submitted with `Ctrl+Enter` (default `false`). |
| `type` | `"string"` \| `"int"` \| `"bool"` | no | Parses the value before storing it in `input` (default `"string"`). |
| `choices` | array of strings | no | Fixed list rendered as a selectable list instead of a free-text field. |

## Task failure

When a run ends in `failed`, the task carries two failure fields:

- `error` — a human-readable message, safe to display.
- `failure` — a machine-readable classification for controllers:

  | Field | Type | Description |
  |-------|------|-------------|
  | `kind` | string | One of `agent`, `infrastructure`, `timeout`, `invalid_input`, `dependency`, `cancelled`, `blocked`, `unknown`. |
  | `message` | string | The same detail as `error`. |
  | `retryable` | bool | Whether an unattended retry could plausibly help. `true` by default for `infrastructure` and `timeout`; `false` for every other kind, including `agent`, `invalid_input`, and `blocked`. |

`failure` is absent (`null`) on success and on rows written before typed
failures existed. `blocked` is a failure kind, not a task status: a blocked run
is terminal and is never retried automatically.

This is part of the runtime task API (the daemon, remote API, and TUI), not a
task-file header key.

## Result envelope

`task.output` stays arbitrary JSON, but every run carries an additive `envelope`
so a dependent task (or a controller) can consume a predecessor's result without
parsing a raw transcript. The existing keys (`agent`, `session_id`,
`session_title`, `output_bytes`, `truncated`, `output`, `result`) are unchanged:

```json
{
  "agent": "opencode",
  "session_id": "ses_…",
  "output_bytes": 12345,
  "truncated": false,
  "output": "<capped raw transcript>",
  "result": { "…": "agent-specific parsed result, or null" },
  "envelope": {
    "summary": "Implemented GitHub webhook signature verification.",
    "artifacts": [{ "kind": "source", "path": "src/github.rs" }],
    "findings": [],
    "outputs": { "tests_passed": true },
    "continuation": null
  }
}
```

| Field | Type | Description |
|-------|------|-------------|
| `summary` | string | Always present. A one-line result summary. |
| `artifacts` | array | Things the run produced; may be empty. |
| `findings` | array | Observations the successor should know; may be empty. |
| `outputs` | object (any JSON) | Structured values keyed by name; defaults to `{}`. |
| `continuation` | object or `null` | Optional machine-readable hint for what to do next. |

A structured agent result may carry the envelope directly (a `summary` string),
under an explicit `envelope` wrapper, or as JSON in its `text` field. When it
does, it is embedded verbatim (normalized to the five keys above). Otherwise
`summary` is synthesised from the tail of the capped raw output and the arrays
are empty. Envelopes are a convention, not a mandatory schema: a result that
does not match simply gets a synthesised summary.

When the parsed result *is* the envelope, `result` is `null` (the envelope is the
canonical copy), matching the existing `{ "text": raw }` de-duplication. Older
rows written before this convention have no `envelope`; consumers must tolerate
`null`. The envelope is additive and never removes existing keys.

**Artifact durability.** A run's worktree is removed unless `keep_worktree` is
set or the agent committed/pushed, so an artifact path must be **repo-relative**
and should not point at a worktree-local file that will be deleted. Prefer
durable `branch`/`commit`/`pr` artifacts, for example
`{"kind":"commit","ref":"favetto/implement-abc12345"}` or
`{"kind":"pr","ref":"#146"}`, over a bare file path.

When the output is embedded in a dependent's `_prev`, `envelope.summary` and any
oversized `artifacts`/`findings`/`outputs` are bounded so the rendered prompt
stays under the argument-size limit. Read the summary with
<span v-pre>`{{ prev.output.envelope.summary }}`</span>.

## Prompt templates

Prompt bodies and `spawn_file` paths are rendered before use.
<span v-pre>`{{ dotted.path }}`</span> placeholders resolve against a JSON
context:

| Placeholder | Resolves to |
|-------------|-------------|
| <span v-pre>`{{ task.id }}`</span> | The task's UUID. |
| <span v-pre>`{{ task.name }}`</span> | The task's catalog identity (relative path, no `.md`). |
| <span v-pre>`{{ input.<name> }}`</span> | A collected variable or the input produced by `spawn`/webhooks. |
| <span v-pre>`{{ prev.* }}`</span> | The result of the `needs` predecessor (also `input._prev`). For `<task>:all_finished`, `prev` is the fan-in aggregate (`root`, `target`, `count`, `succeeded`, `failed`, `cancelled`, `tasks`). |

Strings render raw, objects and arrays as compact JSON, and missing paths as
empty. Double braces leave single braces in prompt code blocks alone.

## Identity and discovery

The daemon loads the catalog **recursively** from `--tasks-dir` (default
`tasks/`). A task's identity is its path relative to that root, without the `.md`
extension, with `/` separators: `tasks/pipelines/plan.md` is the task
`pipelines/plan`, while a root-level `tasks/triage.md` keeps the bare stem
`triage`. Two files with the same stem in different folders are distinct tasks.

That identity is used consistently by `tasks.start` and `catalog.get`,
`needs`/`spawn`, webhook `rule.task`, `catalog:<name>` schedule ids, and
<span v-pre>`{{ task.name }}`</span>. **Moving a task into a subfolder changes
its identity**: an
existing `catalog:<old>` schedule is dropped and recreated, and any `needs`,
`spawn`, or webhook rule that named the old name must be updated.
