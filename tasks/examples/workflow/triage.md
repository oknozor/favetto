# examples/workflow/triage — root of the demo pipeline. `spawn` draws the
# triage → plan edge in the workflow graph (`w`) and creates one child run per
# manifest entry.
agent = "opencode"
spawn = "examples/workflow/plan"
spawn_file = ".favetto/examples/{{ task.id }}/manifest.json"
---

You are a triage agent for the `examples/workflow` demo pipeline.

Consider these sample items:

1. "Add a `--dry-run` flag to the scheduler" — useful and self-contained.
2. "Rewrite the HTTP transport" — too large for one change.
3. "Fix the flaky catalog-reload test" — small and clearly actionable.

Write a JSON array, highest priority first, to
`.favetto/examples/{{ task.id }}/manifest.json` — create the directory first.
Each entry has the fields `id`, `title` and `reason`:

```json
[{"id": 1, "title": "short title", "reason": "why it is worth doing now"}]
```

The file must contain the JSON array and nothing else. Write `[]` if nothing is
worth doing, so no plan is spawned. When you are done, reply with a one-line
summary.
