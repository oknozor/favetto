# Task file format

A catalog task is a Markdown file under the tasks directory (default
`~/.config/favetto/tasks`) whose first section is a TOML header and whose body is
the prompt. Everything
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
| `kind` | `"agent"` \| `"action"` | no | How the task runs. `"agent"` (default) launches an external agent CLI; `"action"` runs a built-in in-daemon step. See [Action tasks](#action-tasks-kind-action). |
| `action` | table | for `kind = "action"` | The `[action]` table naming and configuring the built-in step. Required with `kind = "action"`; rejected otherwise. |
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
| `worktree` | bool | no | Per-task opt-out of `[executor].worktree` isolation. `false` runs the task directly in its base directory, serialized per directory, and never creates a `favetto/*` branch or linked worktree — set it on read-only tasks (triage, review, planning). `true`/absent inherit the global setting; a `kind = "action"` task defaults to `false` instead. |
| `[[vars]]` | array of tables | no | Manual input variables; see below. |

An unknown or invalid key makes the whole file fail to parse, and the running
daemon keeps the previous definition of that task (a half-written edit never
drops a task).

## Action tasks (`kind = "action"`)

A task with `kind = "action"` runs a deterministic built-in step inside the
daemon instead of launching an agent. It is useful for poll-and-decide
pipelines: fetch some state, test it, and only `spawn` the next task when the
condition holds. No agent or session is started, and no worktree is created
unless the task explicitly sets `worktree = true`.

An action task must not set `agent`, `provider`, or `model`, and must declare an
`[action]` table with an `id`:

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
`agent = null`), so `needs` successors and <span v-pre>`{{ prev.* }}`</span> keep
working and `envelope.outputs` carries the action's structured result.

### `http.request`

`[action.params]` for `http.request`:

| Key | Type | Description |
|-----|------|-------------|
| `method` | string | HTTP method; default `GET`. |
| `url` | string | Absolute request URL (required). |
| `query` | table | Query parameters appended to the URL. |
| `headers` | table | Request headers. Values may interpolate `$VAR` / `${VAR}` from the daemon's environment, so tokens are never stored in the task file; a referenced variable that is unset fails the action. |
| `body` | string or value | Request body; strings are sent verbatim, other values as JSON. |
| `timeout_ms` | integer | Whole-request timeout in milliseconds; default `30000`. |
| `fail_on_error` | bool | Fail the action on a non-2xx response; default `true`. Set `false` to record the `status` and branch on it via `[action.when]`. |
| `user_agent` | string | `User-Agent` header; default `favetto/<version>`. |

The action's `outputs` are:

| Field | Description |
|-------|-------------|
| `status` | The HTTP status code. A non-2xx response **fails** the action by default (message carries the status and a bounded body excerpt); set `fail_on_error = false` to branch on `status` instead. |
| `headers` | Response headers, keyed by their lowercased names. |
| `body` | The parsed JSON body, or the raw text when the body is not JSON. |
| `items` | The body when it is an array, else the body's `items` array, else `null`. |
| `truncated` | Whether the body was capped at `[executor].max_output_bytes`. |

Every request sends a `User-Agent` (`favetto/<version>` unless overridden by the
`user_agent` param or a `headers.User-Agent`), which GitHub and other
UA-strict APIs require. A non-2xx response fails the action with an
infrastructure failure, so a rejected request surfaces as a failed task instead
of a silent success.

### Gating `spawn` with `[action.when]`

`[action.when]` is a small, non-Turing-complete predicate over the action's
`outputs`. `path` is a dot path (`items.0.number`) or a `/`-prefixed JSON
Pointer (`/items/0/number`); a missing path resolves to `null`. The `when` table
lives under `[action]`, so `kind = "action"` must also be set.

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
`~/.config/favetto/tasks`). A task's identity is its path relative to that root,
without the `.md` extension, with `/` separators: `pipelines/plan.md` is the task
`pipelines/plan`, while a root-level `triage.md` keeps the bare stem `triage`.
Two files with the same stem in different folders are distinct tasks.

That identity is used consistently by `tasks.start` and `catalog.get`,
`needs`/`spawn`, webhook `rule.task`, `catalog:<name>` schedule ids, and
<span v-pre>`{{ task.name }}`</span>. **Moving a task into a subfolder changes
its identity**: an
existing `catalog:<old>` schedule is dropped and recreated, and any `needs`,
`spawn`, or webhook rule that named the old name must be updated.

### Repository-local tasks

In addition to the global tasks directory, the daemon loads every registered
repository's `<repo>/.favetto/tasks/` catalog. A repo-local task's identity is
prefixed with `repo:<name>/`, where `<name>` is the repository's display name:

```text
<repo>/.favetto/tasks/triage.md            →  repo:<name>/triage
<repo>/.favetto/tasks/pipelines/plan.md    →  repo:<name>/pipelines/plan
```

The `repo:` prefix is reserved, so a global task path may not start with it.
Repo-local tasks default their `cwd` to the repository root. Bare `needs`/`spawn`
references inside a repo-local task resolve to the same repo's task when it
exists (preserving any `:finished`/`:all_finished` suffix); cross-repo references
must be spelled out in full. See [Repository-local tasks](../guide/catalog#repository-local-tasks).
