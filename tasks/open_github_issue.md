agent = "opencode"
provider = "deepseek"
model = "deepseek-v4-flash"
cwd = "/code/che"

[[vars]]
name = "issue_description"
prompt = "Describe the issue to file"
multiline = true
required = true

[[vars]]
name = "repo"
prompt = "Target repository (owner/name)"
default = "oknozor/favetto"
required = true
---

You are filing a GitHub issue with the GitHub MCP server.

Target repository: `{{ input.repo }}`.

Derive a concise, specific title from the description below, then use the MCP to
create the issue with that title and the description as the body.

## Description

{{ input.issue_description }}

Apply the `triage` label to the new issue and report the issue URL when you are
done.
