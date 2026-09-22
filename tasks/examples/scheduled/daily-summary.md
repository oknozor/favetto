# examples/scheduled/daily-summary — a recurring task; the daemon registers it
# as the `catalog:examples/scheduled/daily-summary` schedule (scheduler screen).
agent = "opencode"
schedule = "0 0 8 * * *"
---

Summarise the activity of the last day: new issues, merged pull requests and
any failed runs. Write a short Markdown digest to `reports/daily.md` and reply
with the highlights.
