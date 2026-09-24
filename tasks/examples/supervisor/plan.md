# examples/supervisor/plan — first step of the supervisor-driven two-step demo.
# There is deliberately no `spawn`/`needs` edge to `examples/supervisor/implement`:
# the reference supervisor (`crates/favetto-tui/examples/reference_supervisor.rs`)
# owns the sequencing, so the daemon stays a deterministic executor.
agent = "opencode"
---

You are the planning step of the `examples/supervisor` demo pipeline.

Summarise the single change worth making and write a short, concrete plan to
`.favetto/examples/supervisor/plan.md`. Create the directory first if it does
not exist. Keep it to a few bullet points.

When you are done, reply with a one-line summary of the plan. Do not make any
code changes.
