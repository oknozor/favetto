# examples/variables — the variables form (screenshot `tui-vars-form`).
# Declares one var per field type: multiline text, required text with a default,
# int, choices and bool.
agent = "opencode"
provider = "deepseek"
model = "deepseek-flash"

[[vars]]
name = "issue"
prompt = "Describe the issue"
multiline = true
required = true

[[vars]]
name = "repo"
prompt = "Target repository (owner/name)"
default = "oknozor/favetto"
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

Issue description:

{{ input.issue }}

Review up to {{ input.count }} open issues and write a {{ input.flavor }} report.
When `open_issue` is true, file the report as a new issue; otherwise reply with
it and stop.
