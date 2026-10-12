# Task file format

A task is a `*.md` file. The parser reads lines until the first line that is
exactly `---` (after trimming); those lines are parsed as TOML. Everything after
the separator is the prompt body, trimmed.

```md
agent = "opencode"
provider = "my-provider"
model = "some-model"
cwd = "/path/to/checkout"

[[vars]]
name = "issue"
prompt = "Which issue?"
required = true
---
You are an agent. Handle {{ input.issue }}.
```

If there is no `---`, the whole file is the TOML header and the prompt is empty.

## Header keys

| Key | Type | Required | Description |
|-----|------|----------|-------------|
| `agent` | string | no | Name of an `[agents.*]` config entry. Overrides the global default. A task with no agent anywhere fails to start. |
| `provider` | string | no | Model provider, substituted as `{provider}`. Ignored unless `model` is also set. |
| `model` | string | no | Model id, substituted as `{model}`. When set, the agent's `run_args` template is used instead of `headless_args`. |
| `cwd` | string | no | Working directory the agent runs in. Precedence: per-run `input.cwd` → this key → the agent's configured `cwd`. |
| `schedule` | string | no | A cron expression that makes the task recurring. Mutually exclusive with `needs`. |
| `needs` | string | no | Dependency on another task; see [`templating-and-graph.md`](templating-and-graph.md). Mutually exclusive with `schedule`. |
| `spawn` | string | no | Catalog task to enqueue from this task's handoff file after a successful run. Requires `spawn_file`. |
| `spawn_file` | string | no | Path of the JSON handoff file consumed by `spawn`; rendered as a template at run time and read as a JSON array. |
| `spawn_new_root` | bool | no | When true, each `spawn` child becomes its own workflow root instead of a descendant. Requires `spawn`; default `false`. |
| `sign` | `"off"` \| `"ssh"` \| `"gpg"` | no | Per-task override of `[git] signing`. Most specific wins: task → `[agents.<name>.git]` → `[git]`. |
| `worktree` | bool | no | `false` runs the task directly in its base directory (serialized per directory) and never creates a `favetto/*` branch or worktree. `true`/absent inherit `[executor].worktree`. Set `false` on read-only tasks. |
| `[[vars]]` | array of tables | no | Manual input variables; see below. |

Unknown keys are **silently ignored**. Invalid values (malformed TOML, a bad
`needs` suffix, an invalid `[[vars]]` entry) make the whole file fail to parse,
and the running daemon keeps the task's previous definition rather than dropping
it.

## `[[vars]]`

Declares fields the TUI prompts for before starting the task. Each collected
value becomes `input.<name>`.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | yes | The `input` key. Must match `[a-zA-Z0-9_]+`, be unique in the file, and never be `_prev`. |
| `prompt` | string | yes | The label/question shown in the form. Must be non-empty. |
| `default` | string | no | Value pre-filled in the form. |
| `required` | bool | no | Block submission while empty; unattended runs fail when the value is absent. Default `false`. |
| `multiline` | bool | no | `Enter` inserts a newline and the form is submitted with `Ctrl+Enter`. Default `false`. |
| `type` | `"string"` \| `"int"` \| `"bool"` | no | Parses the value before storing it in `input`. Default `"string"`. |
| `choices` | array of strings | no | Fixed list rendered as a selectable list instead of free text. |

`int` values must parse as integers and `bool` values accept `true`/`false`
(case-insensitive) or `1`/`0`; a value outside `choices` is rejected.

## Identity and discovery

The daemon loads `*.md` files **recursively** from the tasks root (default
`tasks/`, set with `--tasks-dir` or `[daemon].tasks_dir`). A task's identity is
its path relative to that root, without `.md`, with `/` separators:

```text
tasks/triage.md              →  triage
tasks/release/changelog.md   →  release/changelog
```

The same identity is used by `tasks.start`, `catalog.get`, `needs`/`spawn`,
webhook `rule.task`, `catalog:<name>` schedules, and `{{ task.name }}`. Two files
with the same stem in different folders are distinct tasks. **Moving a task into
a subfolder changes its identity** and therefore breaks anything that named the
old identity.

## Validation

```sh
favetto validate tasks/release/changelog.md
```

Exits 0 and prints `ok: <name> (agent=<agent>)` on success, or exits non-zero
with the parse error. `--name` overrides the identity used for validation; with
no file argument the definition is read from stdin. The CLI is the offline
counterpart of the daemon's catalog load and uses the same parser, so a file that
validates here loads identically.
