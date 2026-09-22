# examples/workflow/report — a `needs` successor; draws the triage → report
# dependency edge and reads the predecessor result through `{{ prev.* }}`.
agent = "opencode"
needs = "examples/workflow/triage:finished"
---

A triage run just finished. Its output was:

{{ prev.output }}

Summarise it in three bullets and suggest the single item to do first.
