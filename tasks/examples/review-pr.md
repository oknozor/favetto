# examples/review-pr — an int + choices form, and a `provider`/`model`
# selection so the preview shows the model headers (Agents guide).
agent = "opencode"
provider = "deepseek"
model = "deepseek-v4-flash"

[[vars]]
name = "pr_number"
prompt = "Pull request number"
type = "int"
required = true

[[vars]]
name = "repo"
prompt = "Target repository (owner/name)"
default = "oknozor/favetto"
required = true

[[vars]]
name = "focus"
prompt = "Review focus"
choices = ["correctness", "security", "performance", "tests"]
default = "correctness"

[[vars]]
name = "post_review"
prompt = "Post the review to GitHub"
type = "bool"
default = "true"
---

Review `{{ input.repo }}#{{ input.pr_number }}` with a focus on
{{ input.focus }}.

Run the project's checks locally and base every finding on the diff or on a
command you actually ran. When `post_review` is true, post one comment review;
otherwise reply with the review text and stop. Do not merge or approve the pull
request.
