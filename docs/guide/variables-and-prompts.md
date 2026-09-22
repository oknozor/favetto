# Variables and prompts

Tasks often need a few values from you before they run: which issue, which
repository, how many items. Declare them with `[[vars]]` and favetto prompts for
them when you start the task, then substitutes them into the prompt.

## Prompt for input

Add a `[[vars]]` table per value. This task asks for an issue description and a
target repository, with a sensible default:

```md
agent = "opencode"

[[vars]]
name = "issue"
prompt = "Describe the issue"
multiline = true
required = true

[[vars]]
name = "repo"
prompt = "Target repository"
default = "oknozor/favetto"
---
Open an issue in {{ input.repo }} with this body:

{{ input.issue }}
```

Starting it from the Catalog tab opens a floating form. `Enter` advances to the
next field and submits on the last one; a multiline field inserts a newline on
`Enter`, so submit with `Ctrl+Enter`. `Esc` cancels and starts nothing.

![favetto TUI — variables form](/screenshots/tui-vars-form.svg)

*The `[[vars]]` form opened by starting a task.*

The values become the task's `input`, so <span v-pre>`{{ input.issue }}`</span>
and <span v-pre>`{{ input.repo }}`</span> render in the prompt.

### Variable fields

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | The `input` key. `[a-zA-Z0-9_]+`, unique, never `_prev`. |
| `prompt` | string | The label shown in the form. |
| `default` | string | Value pre-filled in the form. |
| `required` | bool | Block submission while empty (default `false`). |
| `multiline` | bool | `Enter` inserts a newline; submit with `Ctrl+Enter` (default `false`). |
| `type` | `string` \| `int` \| `bool` | Parse the value before storing it (default `string`). |
| `choices` | array | Render a selectable list instead of a free-text field. |

A `type = "int"` value is stored as a JSON number, so
<span v-pre>`{{ input.count }}`</span> renders `3`, not `"3"`. A non-numeric entry blocks submission. `choices` is
validated too:

```toml
[[vars]]
name = "flavor"
prompt = "Flavor"
choices = ["vanilla", "mint"]
```

An invalid declaration (empty name, a duplicate, `_prev`, or an empty prompt)
makes the whole file fail to parse, so the live catalog keeps the previous
definition.

## Prompt templates

Prompt bodies and `spawn_file` paths are rendered before use.
<span v-pre>`{{ dotted.path }}`</span> placeholders resolve against a JSON context:

| Placeholder | Resolves to |
|-------------|-------------|
| <span v-pre>`{{ task.id }}`</span> | The task's UUID. |
| <span v-pre>`{{ task.name }}`</span> | The task's catalog identity. |
| <span v-pre>`{{ input.<name> }}`</span> | A collected variable, a webhook summary field, or a spawned element. |
| <span v-pre>`{{ prev.* }}`</span> | The result of the `needs` predecessor. |

Strings render raw, objects and arrays render as compact JSON, and missing paths
render empty. Double braces leave single braces in prompt code blocks alone, so
a shell `${VAR}` or a Rust `{value}` inside a fenced block is untouched.

::: tip Variables only prompt interactive starts
The form never appears for spawned children, `needs` successors, cron runs, or
raw `tasks.start` calls. Pass those values in the element or payload instead. A
`required` variable that is absent makes the unattended run fail with
`task '<name>' requires input variable '<var>'` rather than starting with an
empty value.
:::

Next: [Schedules and dependencies](./schedules-and-dependencies).
