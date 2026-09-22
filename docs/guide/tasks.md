# Tasks & catalog

A task is a Markdown file under `tasks/` (at the root or in any subfolder) with a
TOML header and the prompt as the body:

```md
agent = "opencode"                      # optional — run through this external agent
provider = "jev"                        # optional — model provider
model = "1.13"                          # optional — model id (uses the agent's run_args)
cwd = "/code/che"                       # optional — repo/checkout the agent works in
schedule = "0 8 * * * *"                # optional — makes this a recurring task
needs = "another_task:finished"         # optional — start when `another_task` ends
spawn = "child_task"                    # optional — fan out from a handoff file
spawn_file = ".favetto/{{ task.id }}/manifest.json"  # JSON array → one child per item
---

You are an engineering agent that implements Linear tickets end-to-end.
…
```

- `agent` names an `[agents.*]` entry (see [Embedded agents](./agents)). When set
  — or when `[agent].default` is configured — the task runs through that agent CLI
  with the task prompt (headless for catalog runs, interactive in the Agent tab).
  A task with no agent and no default fails to start.
- `provider` and `model` select the model per task. When `model` is set the
  agent's `run_args` template is used (`{provider}`/`{model}` substituted);
  otherwise the agent's `headless_args` are used and the agent's own default model
  applies. A `provider` without a `model` is ignored.
- `cwd` is the directory the agent runs in (e.g. a repo checkout); it takes
  precedence over the agent's configured `cwd`. Per-run `tasks.start` `input.cwd`
  wins over both.
- `schedule` registers a recurring cron task.
- `needs` declares a dependency: this task auto-starts when the named task emits
  its `finished` event (mutually exclusive with `schedule`). The finished task's
  result is attached as `input._prev`, reachable in the prompt as
  <span v-pre>`{{ prev.output }}`</span> / <span v-pre>`{{ prev.session_id }}`</span>.
- `spawn` + `spawn_file` chain tasks: when this task succeeds, the JSON file at
  `spawn_file` is read and its array is fanned out — one `spawn`-named task is
  enqueued per element, with that element as its `input`. An empty array spawns
  nothing, so a task can decline to continue.

## Input variables (`[[vars]]`)

A task can declare manual input variables in the TOML header. Starting it from
the TUI opens a floating form that prompts for each one; the collected values
become the task's `input` and render through
<span v-pre>`{{ input.<name> }}`</span> exactly like
the values produced by `spawn`/`needs`:

```toml
agent = "opencode"

[[vars]]
name = "issue_description"            # becomes input.issue_description
prompt = "Describe the issue to file" # label shown in the form
multiline = true                      # Enter inserts a newline; Ctrl+Enter submits
required = true

[[vars]]
name = "repo"
prompt = "Target repository"
default = "oknozor/favetto"
required = false

[[vars]]
name = "count"
prompt = "How many items"
type = "int"                          # string (default) | int | bool

[[vars]]
name = "flavor"
prompt = "Flavor"
choices = ["vanilla", "mint"]         # rendered as a selectable list
---
```

- `name` (required) — the `input` key. Must match `[a-zA-Z0-9_]+`, be unique
  within the file, and must not be `_prev` (reserved for the `needs`
  predecessor). An invalid declaration makes the whole file fail to parse, so
  the live catalog keeps the previous definition.
- `prompt` (required) — the label/question shown in the form.
- `default` (optional) — the value pre-filled in the form.
- `required` (optional, default `false`) — submission is blocked while the field
  is empty, and unattended runs (hook / `needs` / `spawn` / cron / raw RPC) fail
  with `task '<name>' requires input variable '<var>'` when it is absent.
- `multiline` (optional, default `false`) — `Enter` inserts a newline and the
  form is submitted with `Ctrl+Enter` (or `Tab` from the last field).
- `type` (optional, default `"string"`; one of `string`, `int`, `bool`) — the
  value is parsed before being stored, so <span v-pre>`{{ input.count }}`</span> renders a JSON
  number/bool rather than a quoted string. A non-numeric `int` blocks submission.
- `choices` (optional) — a fixed list rendered as a selectable list instead of a
  free-text field.

Variables share the `input` namespace: omit an optional field and its key is
left out, so <span v-pre>`{{ input.x }}`</span> renders empty (the existing behavior). The form
never prompts for spawned children or other unattended starts — pass the values
in the element/payload instead.

## Prompt templates

Task prompt bodies and `spawn_file` paths are rendered before use.
<span v-pre>`{{ dotted.path }}`</span> placeholders resolve against a JSON
context: <span v-pre>`{{ task.id }}`</span>, <span v-pre>`{{ task.name }}`</span>,
<span v-pre>`{{ input.* }}`</span> (the task's input JSON), and
<span v-pre>`{{ prev.* }}`</span> (the `needs`
predecessor; `input._prev` is reserved). Strings render raw, objects/arrays as
compact JSON, and missing paths as empty. Double braces leave single braces in
prompt code blocks alone.

This makes a triage → plan → implement pipeline declarative: the triage task
writes a `manifest.json` array of issues and `spawn`s the planning task once per
issue; each planning task writes a `handoff.json` and `spawn`s the implementation
task. See `tasks/triage_cocogitto_issues.md`, `tasks/plan_cocogitto_issue.md`, and
`tasks/implement_cocogitto_issue.md`. Favetto ships the same pipeline for its own
issues in `tasks/triage_favetto_issues.md`, `tasks/plan_favetto_issue.md`, and
`tasks/implement_favetto_issue.md`.

## Catalog reload & preview

The daemon loads the catalog from `--tasks-dir` (default `tasks/`)
**recursively**: tasks may live at the root or in any subfolder, and a task's
identity is its path relative to that root, without the `.md` extension, with `/`
separators. So `tasks/pipelines/plan.md` is the task `pipelines/plan`, while a
root-level `tasks/triage.md` keeps its bare stem `triage`. That same relative
path is what `tasks.start`/`catalog.get` take, what `catalog.add`'s `name` may
contain (type `pipelines/plan` to create the folder), and what `needs`/`spawn`,
webhook `rule.task`, `catalog:<name>` schedule ids and <span v-pre>`{{ task.name }}`</span>
refer to — so two files with the same stem in different folders are distinct
tasks. **Moving a task into a subfolder changes its identity**: an existing
`catalog:<old>` schedule is dropped and recreated, and any `needs`/`spawn` or
webhook rule that named the bare stem must be updated to the relative path.

The daemon watches that directory and its subfolders (via `notify`, with a 200 ms
debounce): adding, editing, or removing a `.md` file — at the root or nested —
reloads the catalog on the fly. A file that fails to parse keeps its previous
definition (so a half-written edit never drops a task), and the recurring tasks
derived from `schedule` headers are re-synced — a changed cron is re-registered and
a removed header deletes the `catalog:<name>` schedule, while manually created
schedules are left alone. Every reload pushes `catalog.updated` so attached TUIs
re-fetch the list and refresh the preview. Runs already in flight keep the
definition they were dispatched with; a queued task picks up the freshly loaded
definition when it starts. From the TUI, the **Catalog** tab renders the catalog as
a collapsible folder/task tree; Enter starts a task and a task that declares
`[[vars]]` opens a floating form first (Enter advances, `Ctrl+Enter` submits,
`Esc` cancels and starts nothing), while a var-free task starts immediately. The
Ctrl+P menu's "Add task" writes a new `.md` file (it does not run it). Task
lifecycle emits `task_idle` / `task_started` / `task_finished` events.

The Catalog tree shows a folder glyph (`📂`/`📁`) before each folder and a file
glyph (`📄`) before each task, indented by depth. Fold a folder with a mouse click
on its row, `Space`, or `Enter` on the folder; clicking a task selects it and a
second click (or `Enter`) starts it. `Enter` on a folder only folds/unfolds, never
starts a task. Terminals that render the emoji as double width can set
`FAVETTO_PLAIN_ICONS=1` for an ASCII fallback (`[-]`/`[+]`/`-`).

The Catalog tab shows a **preview side panel** for the highlighted task: its raw
`.md` source with the TOML front-matter highlighted as TOML and the prompt rendered
as Markdown (headings, lists, blockquotes, fenced code, inline code/bold/italic/
links). Moving the selection with ↑/↓ loads the preview for the new task (a folder
row clears it).

The Events tab is a selectable, scrolling list (↑/↓, PageUp/PageDown) with a
**payload side panel** that pretty-prints the selected event's JSON with syntax
highlighting (keys, strings, numbers, booleans/null).
