# Webhooks & hooks

GitHub signs with `X-Hub-Signature-256`; Linear signs with `Linear-Signature`.
Both are HMAC-SHA256 over the raw body, verified in constant time.

## GitHub triggers (config-driven)

Declare trigger rules in `config.toml`; GitHub POSTs a signed event to
`/webhooks/github` and every matching rule enqueues its catalog task with a
**truncated summary** of the event as the task's `input`:

```toml
[webhook.github]
enabled = true
secret_env = "GITHUB_WEBHOOK_SECRET"   # preferred; or `secret = "…"` (discouraged)

[[webhook.github.rules]]
name = "triage-opened-issues"
event = "issues"                       # X-GitHub-Event
action = "opened"                      # optional; unset matches any action
task = "triage_favetto_issues"         # catalog task to enqueue
filter = { repo = "oknozor/*" }        # optional

[[webhook.github.rules]]
name = "plan-bug-prs"
event = "pull_request"
action = "opened"
task = "plan_favetto_issue"
filter = { labels_contains = ["bug"], base_ref = "main", author = "oknozor" }
```

- **Secret precedence**: literal `secret`, else the env var named by
  `secret_env` (default `GITHUB_WEBHOOK_SECRET`). Linear always reads
  `LINEAR_WEBHOOK_SECRET`. A configured GitHub secret without `enabled = true`
  leaves the endpoint disabled (404).
- **Supported events**: `issues`, `issue_comment`, `pull_request`,
  `pull_request_review`, `push`, `workflow_run`, `check_suite`, `check_run`
  (the issue/PR lifecycle actions map to the `issue_*` / `pr_*` event kinds;
  `workflow_run/completed` reuses `action_run_completed`). Unrecognised events
  are ack'd with 200 and ignored; a rule naming an unsupported event, an unknown
  catalog task, or an invalid glob is a **startup error**.
- **Summary fields** (never the raw body; title capped at 256 bytes, body at
  1024, truncated on UTF-8 boundaries): `event`, `action`, `repo`, `repo_owner`,
  `repo_name`, `number`, `title`, `body`, `author`, `html_url`, `labels`,
  `base_ref`, `head_ref`, `ref`, `commit_count`, `installation_id`,
  `organization`. Prompts reach them as <span v-pre>`{{ input.repo }}`</span>,
  <span v-pre>`{{ input.number }}`</span>, <span v-pre>`{{ input.title }}`</span>,
  <span v-pre>`{{ input.author }}`</span>, <span v-pre>`{{ input.labels }}`</span>,
  ….
- **Filters**: `repo` / `author` / `base_ref` / `head_ref` are globs (`*` also
  crosses `/`, so `oknozor/*` matches `oknozor/favetto`); `labels_contains` is
  any-of exact. Unset fields match anything; all set fields must match (AND).
- **Idempotency**: deliveries are deduplicated on `X-GitHub-Delivery`, and each
  enqueued task gets a delivery-scoped dedupe key, so a redelivery never
  double-runs a task.
- **Restart required**: rules and the secret are read at daemon startup; editing
  `config.toml` needs a restart (there is no config hot-reload).

## Local testing

```bash
# terminal 1
GITHUB_WEBHOOK_SECRET=topsecret ./target/debug/favetto daemon

# terminal 2 — forward real events (needs the GitHub CLI)
gh webhook forward --events=issues --url=http://127.0.0.1:7878/webhooks/github

# or craft a signed delivery with curl + openssl
BODY='{"action":"opened","issue":{"number":1,"title":"hi"},"repository":{"full_name":"oknozor/favetto"},"sender":{"login":"oknozor"}}'
SIG=$(printf '%s' "$BODY" | openssl dgst -sha256 -hmac topsecret | awk '{print $2}')
curl -X POST http://127.0.0.1:7878/webhooks/github \
  -H "X-GitHub-Event: issues" -H "X-GitHub-Delivery: local-1" \
  -H "X-Hub-Signature-256: sha256=$SIG" -d "$BODY"
```

## Notification hooks

Notification hooks are **in-memory**: the TUI adds them via the Ctrl+P "Create a
notification (hook)" wizard (the `hooks.upsert` RPC). They run against every
persisted event and `notify` sends through a channel (e.g. `channel = "webhook"`,
`config = { url = "http://…" }`). They are not persisted and are lost on restart;
task triggers are configured declaratively with `[webhook.github]` above.
