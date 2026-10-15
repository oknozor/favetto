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
| `kind` | `"agent"` \| `"action"` | no | How the task runs. `"agent"` (default) launches an external agent CLI; `"action"` runs a built-in in-daemon step. See [Action tasks](#action-tasks-kind-action). |
| `action` | table | for `kind = "action"` | The `[action]` table naming and configuring the built-in step. Required with `kind = "action"`; rejected otherwise. |
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
| `worktree` | bool | no | `false` runs the task directly in its base directory (serialized per directory) and never creates a `favetto/*` branch or worktree. `true`/absent inherit `[executor].worktree`, except a `kind = "action"` task defaults to `false`. Set `false` on read-only tasks. |
| `[[vars]]` | array of tables | no | Manual input variables; see below. |

Unknown keys are **silently ignored**. Invalid values (malformed TOML, a bad
`needs` suffix, an invalid `[[vars]]` entry) make the whole file fail to parse,
and the running daemon keeps the task's previous definition rather than dropping
it.

## Action tasks (`kind = "action"`)

`kind = "action"` runs a deterministic built-in step inside the daemon instead of
launching an agent. It is for poll-and-decide pipelines: fetch some state, test
it, and only `spawn` the next task when the condition holds. No agent or session
is started, and no worktree is created unless the task sets `worktree = true`.

An action task must **not** set `agent`, `provider`, or `model`, and must declare
an `[action]` table with an `id`:

```md
kind = "action"
schedule = "0 */15 * * * *"
worktree = false
spawn = "triage_issues"
spawn_file = ".favetto/gates/{{ task.id }}/handoff.json"

[action]
id = "http.request"

[action.params]
method = "GET"
url = "https://api.github.com/repos/oknozor/favetto/issues"
query = { state = "open" }
headers = { Accept = "application/vnd.github+json", Authorization = "Bearer $GITHUB_TOKEN" }
timeout_ms = 15000

[action.when]
path = "items"
op = "not_empty"
---
Poll the GitHub issues API; when open issues exist, start `triage_issues`.
```

| Key | Type | Required | Description |
|-----|------|----------|-------------|
| `[action].id` | string | yes | The built-in action id. `http.request` is the only built-in today. |
| `[action.params]` | table | no | Action parameters; see `http.request` below. |
| `[action.when]` | table | no | Predicate over the action result that gates `spawn`. |
| `[action.spawn_input]` | value | no | JSON value used as the single spawned child's `input`; defaults to `{}`. |

The result is folded into the same envelope an agent run produces (with
`agent = null`), so `needs` successors and `{{ prev.* }}` keep working and
`envelope.outputs` carries the action's structured result.

### `http.request`

| `[action.params]` key | Type | Description |
|-----------------------|------|-------------|
| `method` | string | HTTP method; default `GET`. |
| `url` | string | Absolute request URL (required). |
| `query` | table | Query parameters appended to the URL. |
| `headers` | table | Request headers. Values may interpolate `$VAR` / `${VAR}` from the daemon's environment, so tokens are never stored in the task file; a referenced variable that is unset fails the action. |
| `body` | string or value | Request body; strings are sent verbatim, other values as JSON. |
| `timeout_ms` | integer | Whole-request timeout in milliseconds; default `30000`. |

The action's `outputs` are `status` (a non-2xx response is **not** a failure),
`headers` (lowercased names), `body` (parsed JSON, or raw text when not JSON),
`items` (the body when it is an array, else its `items` array, else `null`), and
`truncated` (whether the body was capped at `[executor].max_output_bytes`).

### Gating `spawn` with `[action.when]`

`[action.when]` is a small, non-Turing-complete predicate over the action's
`outputs`. `path` is a dot path (`items.0.number`) or a `/`-prefixed JSON Pointer
(`/items/0/number`); a missing path resolves to `null`. The `when` table lives
under `[action]`, so `kind = "action"` must also be set.

| `op` | True when the resolved value… | Needs `value` |
|------|-------------------------------|---------------|
| `not_empty` | is non-`null` and non-empty (a non-empty string, array, or object) | no |
| `empty` | is missing, `null`, or empty | no |
| `eq` / `ne` | equals / differs from `value` | yes |
| `gt` / `lt` | is greater / less than `value` (numbers or strings) | yes |
| `contains` | is a string containing `value`, an array holding `value`, or an object with the `value` key | yes |

When the predicate is false the task still **succeeds** but enqueues nothing —
the same outcome as an empty `[]` manifest. When it holds, the runtime writes the
child manifest to the rendered `spawn_file` (`[action.spawn_input]`, default
`[{}]`) and reuses the normal `spawn` lineage, dedupe, and fan-in behaviour.

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
`~/.config/favetto/tasks`, set with `--tasks-dir` or `[daemon].tasks_dir`). A
task's identity is its path relative to that root, without `.md`, with `/`
separators:

```text
triage.md              →  triage
release/changelog.md   →  release/changelog
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
