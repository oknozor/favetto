# Event kinds

Generated from `crates/favetto-core/src/model.rs` by the docs generator. Do not edit by hand.

Every event persisted on the bus is one of the kinds below. The **Event** column is the stable `snake_case` wire and storage form; `from_name` also accepts the spec's `PascalCase` spelling.

| Event | Description |
|-------|-------------|
| `task_created` | A task row was created. |
| `task_updated` | A task row changed. |
| `task_completed` | A task completed successfully (terminal). |
| `task_failed` | A task failed (terminal). |
| `task_cancelled` | A task was cancelled (terminal). |
| `task_idle` | A task was enqueued and is waiting to run. |
| `task_started` | A task began running. |
| `task_finished` | A task ended (success or failure); used by `needs` dependencies. |
| `cron_tick` | A scheduled cron job fired. |
| `issue_created` | An issue was created. |
| `issue_updated` | An issue was edited. |
| `issue_closed` | An issue was closed. |
| `issue_reopened` | An issue was reopened. |
| `issue_labeled` | A label was added to or removed from an issue. |
| `issue_assigned` | An issue was assigned. |
| `issue_comment_created` | A comment was posted on an issue. |
| `pr_created` | A pull request was opened. |
| `pr_merged` | A pull request was merged. |
| `pr_closed` | A pull request was closed without merging. |
| `pr_reopened` | A pull request was reopened. |
| `pr_synchronized` | A pull request received new commits. |
| `pr_ready_for_review` | A draft pull request was marked ready for review. |
| `pr_review_requested` | A review was requested on a pull request. |
| `pr_updated` | A pull request was edited. |
| `pr_review_submitted` | A review was submitted on a pull request. |
| `push_received` | A push was received on a branch. |
| `ticket_created` | A ticket was created (integration event). |
| `ticket_updated` | A ticket was updated (integration event). |
| `action_run_completed` | A workflow run completed. |
| `check_suite_completed` | A check suite completed. |
| `check_run_completed` | A check run completed. |
| `email_received` | An email was received (integration event). |
| `synthetic` | A synthetic marker event used before real integrations exist. |

