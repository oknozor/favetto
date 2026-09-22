# Webhooks and hooks

favetto can trigger catalog tasks from external events. GitHub POSTs a signed
delivery to `/webhooks/github` and every matching rule enqueues its task with a
**truncated summary** of the event as the task's `input`. GitHub signs with
`X-Hub-Signature-256`; Linear signs with `Linear-Signature`. Both are HMAC-SHA256
over the raw body, verified in constant time.

## Trigger a task from GitHub

Declare rules in `config.toml`:

```toml
[webhook.github]
enabled = true
secret_env = "GITHUB_WEBHOOK_SECRET"   # preferred; or `secret = "…"` (discouraged)

[[webhook.github.rules]]
name = "triage-opened-issues"
event = "issues"                       # X-GitHub-Event
action = "opened"                      # optional; unset matches any action
task = "favetto/triage_issues"         # catalog identity
filter = { repo = "oknozor/*" }        # optional

[[webhook.github.rules]]
name = "plan-bug-prs"
event = "pull_request"
action = "opened"
task = "favetto/plan_issue"
filter = { labels_contains = ["bug"], base_ref = "main", author = "oknozor" }
```

`task` is the [catalog identity](./catalog) — the path relative to the tasks
root, without `.md`. Here the rules point at the tasks that ship in
`tasks/favetto/`: `triage_issues` becomes `favetto/triage_issues`.

A rule naming an unsupported event, an unknown catalog task, or an invalid glob
is a **startup error**, so a typo fails the daemon rather than silently never
firing.

### What the task receives

The enqueued task's `input` is a summary, never the raw body:

| Field | Notes |
|-------|-------|
| `event`, `action` | The GitHub event and action. |
| `repo`, `repo_owner`, `repo_name` | Repository identity. |
| `number`, `title`, `body` | Title capped at 256 bytes, body at 1024, truncated on UTF-8 boundaries. |
| `author`, `html_url`, `labels` | Sender and links. |
| `base_ref`, `head_ref`, `ref`, `commit_count` | Branch and push details. |
| `installation_id`, `organization` | GitHub App context. |

Reach them in the prompt with <span v-pre>`{{ input.repo }}`</span>,
<span v-pre>`{{ input.number }}`</span>,
<span v-pre>`{{ input.title }}`</span>,
<span v-pre>`{{ input.labels }}`</span>, and so on.

- **Secret precedence**: literal `secret`, else the env var named by
  `secret_env`, else `GITHUB_WEBHOOK_SECRET`. Linear always reads
  `LINEAR_WEBHOOK_SECRET`. A configured secret without `enabled = true` leaves
  the endpoint disabled (404).
- **Supported events**: `issues`, `issue_comment`, `pull_request`,
  `pull_request_review`, `push`, `workflow_run`, `check_suite`, `check_run`.
  Unrecognised events are acknowledged with 200 and ignored.
- **Filters**: `repo` / `author` / `base_ref` / `head_ref` are globs (`*` also
  crosses `/`, so `oknozor/*` matches `oknozor/favetto`); `labels_contains` is
  any-of exact. Unset fields match anything; all set fields must match (AND).
- **Idempotency**: deliveries are deduplicated on `X-GitHub-Delivery`, and each
  enqueued task gets a delivery-scoped dedupe key, so a redelivery never
  double-runs a task.
- **Restart required**: rules and the secret are read at daemon startup; editing
  `config.toml` needs a restart (there is no config hot-reload).

## Test locally

Forward real events with the GitHub CLI, or craft a signed delivery by hand:

```bash
# terminal 1
GITHUB_WEBHOOK_SECRET=topsecret favetto daemon

# terminal 2 — forward real events (needs the GitHub CLI)
gh webhook forward --events=issues --url=http://127.0.0.1:7878/webhooks/github

# or craft a signed delivery with curl + openssl
BODY='{"action":"opened","issue":{"number":1,"title":"hi"},"repository":{"full_name":"oknozor/favetto"},"sender":{"login":"oknozor"}}'
SIG=$(printf '%s' "$BODY" | openssl dgst -sha256 -hmac topsecret | awk '{print $2}')
curl -X POST http://127.0.0.1:7878/webhooks/github \
  -H "X-GitHub-Event: issues" -H "X-GitHub-Delivery: local-1" \
  -H "X-Hub-Signature-256: sha256=$SIG" -d "$BODY"
```

If the endpoint returns 404, the webhook is not `enabled` or no secret is
configured. If it returns 401, the signature does not match — check the secret
and that you signed the exact body.

## Notification hooks

Notification hooks are **in-memory**: the TUI adds them via the Ctrl+P "Create a
notification (hook)" wizard (the `hooks.upsert` RPC). They run against every
persisted event and `notify` sends through a channel (for example
`channel = "webhook"`, `config = { url = "http://…" }`). They are not persisted
and are lost on restart; task triggers are configured declaratively with
`[webhook.github]` above.

For example, to be notified when a task's agent is blocked on user input:

```text
hooks.upsert { event = "task_awaiting_input", channel = "webhook", config = { url = "http://…" } }
```

`task_awaiting_input` carries `task_id`, `name`, `session_id`, and a `reason`
object (`kind` plus the prompt `message`), so a filter can narrow to a specific
task or prompt kind.

See [Remote API](../reference/remote-api) for the webhook endpoints and
[Environment variables](../reference/environment) for the secret variables.
