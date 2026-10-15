# A deterministic action gate: poll the GitHub issues API every 15 minutes and
# only spawn the triage task when the response is non-empty. No agent run is
# spent on an empty poll. See docs/reference/tasks (Action tasks).
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
