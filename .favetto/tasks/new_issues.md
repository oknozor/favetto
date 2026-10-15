# Poll for new GitHub issues and only start triage when there is something to
# do. A `kind = "action"` task runs the HTTP check deterministically (no agent,
# no worktree) and gates its `spawn` on a non-empty `items` array. See
# docs/reference/tasks (Action tasks) and docs/guide/schedules-and-dependencies.
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
