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
| `needs` | string | no | Dependency of the form `<task>:finished`. This task auto-starts when the named task finishes; the predecessor result is available as <span v-pre>`{{ prev.* }}`</span>. Mutually exclusive with `schedule`. |
| `spawn` | string | no | Catalog task to enqueue from this task's handoff file when this task succeeds. Requires `spawn_file`. |
| `spawn_file` | string | no | Path of the JSON handoff file consumed by `spawn`. Rendered as a template at run time; read as a JSON array, one child per element. |
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

## Prompt templates

Prompt bodies and `spawn_file` paths are rendered before use.
<span v-pre>`{{ dotted.path }}`</span> placeholders resolve against a JSON
context:

| Placeholder | Resolves to |
|-------------|-------------|
| <span v-pre>`{{ task.id }}`</span> | The task's UUID. |
| <span v-pre>`{{ task.name }}`</span> | The task's catalog identity (relative path, no `.md`). |
| <span v-pre>`{{ input.<name> }}`</span> | A collected variable or the input produced by `spawn`/webhooks. |
| <span v-pre>`{{ prev.* }}`</span> | The result of the `needs` predecessor (also `input._prev`). |

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
