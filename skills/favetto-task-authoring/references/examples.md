# Examples

Neutral, copy-ready tasks. Validate any of them with
`favetto validate <path>`.

## Minimal one-shot

The smallest useful task: one agent, one prompt, no inputs.

```md
agent = "opencode"
---
Summarise the open issues in the current repository and reply with the list.
```

## Form with variables

Declare `[[vars]]` when a human supplies input. Each field type is shown:
multiline text, required text with a default, `int`, `choices`, and `bool`.

```md
agent = "opencode"
provider = "my-provider"
model = "my-model"

[[vars]]
name = "issue"
prompt = "Describe the issue"
multiline = true
required = true

[[vars]]
name = "repo"
prompt = "Target repository (owner/name)"
default = "owner/name"
required = true

[[vars]]
name = "count"
prompt = "How many candidates to consider"
type = "int"
default = "5"

[[vars]]
name = "flavor"
prompt = "Report style"
choices = ["concise", "detailed"]
default = "concise"

[[vars]]
name = "open_issue"
prompt = "Open an issue when the report is ready"
type = "bool"
default = "false"
---
You are a triage agent for `{{ input.repo }}`.

Issue: {{ input.issue }}

Review up to {{ input.count }} candidates and write a {{ input.flavor }} report.
When `open_issue` is true, file the report as a new issue; otherwise reply with
it and stop.
```

## Dependency chain (`needs` + `prev`)

`plan` runs first; `implement` starts once a `plan` run succeeds and reads the
predecessor's envelope.

```md
# tasks/pipeline/plan.md
agent = "opencode"
---
Write an implementation plan to `.favetto/plans/plan.md` and finish with a
one-line summary.
```

```md
# tasks/pipeline/implement.md
agent = "opencode"
needs = "pipeline/plan:succeeded"
---
The plan step reported: {{ prev.output.envelope.summary }}

Implement the change it describes in the working tree, run the tests, and reply
with the files changed.
```

## Fan-out handoff (`spawn` + `spawn_file`)

After a successful run, `decompose` reads its handoff array and enqueues one
`pipeline/implement` per element, passing each element as `input`.

```md
# tasks/pipeline/decompose.md
agent = "opencode"
spawn = "pipeline/implement"
spawn_file = ".favetto/plans/{{ input.issue }}/handoff.json"
---
Split issue {{ input.issue }} into independent steps. Write the handoff file as
a JSON array, one object per step:

[
  { "step": 1, "title": "short step title", "detail": "what to change" }
]

Create the directory first, write the array and nothing else, then reply with a
one-line summary.
```

```md
# tasks/pipeline/implement.md — receives one handoff element as input
agent = "opencode"
---
Implement step {{ input.step }}: {{ input.title }}.

{{ input.detail }}

Run the tests and reply with the files changed.
```

## Recurring task (`schedule`)

```md
agent = "opencode"
schedule = "0 0 8 * * *"
---
Summarise the last day of activity: new issues, merged pull requests, and failed
runs. Write a short Markdown digest to `reports/daily.md` and reply with the
highlights.
```

## Poll and gate with an action (`kind = "action"`)

A deterministic gate runs no agent: it polls the GitHub issues API and only
spawns `triage_issues` when the response's `items` array is non-empty. The
`GITHUB_TOKEN` is read from the daemon's environment by the header value, so the
secret never lands in the task file.

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

## Read-only task (`worktree = false`)

Planning, triage, and review tasks should not create a worktree or branch.

```md
agent = "opencode"
worktree = false
---
Review the current tree for unused dependencies and reply with a prioritised
list. Do not modify files.
```
