# examples/scheduled/weekly-digest — a second recurring task, every Monday at
# 09:00 (six-field cron: second minute hour day month weekday).
agent = "opencode"
provider = "opencode-go"
model = "deepseek-v4.1-flash"
schedule = "0 0 9 * * 1"
---

Summarise the last week: what shipped, what slipped and what is blocked. Write
the digest to `reports/weekly.md` and reply with the three most important lines.
