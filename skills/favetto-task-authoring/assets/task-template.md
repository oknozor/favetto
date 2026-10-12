# Copy this file to the tasks root and rename it to `<name>.md`.
# The header is TOML up to the first `---`; the body below is the prompt.
# Docs: the favetto-task-authoring skill references/ directory.

# agent = "opencode"              # an [agents.*] entry; overrides the default
# provider = "my-provider"        # model provider; only used with `model`
# model = "my-model"              # model id; switches to the agent's run_args
# cwd = "/path/to/checkout"       # working directory for the run
# schedule = "0 0 8 * * *"        # recurring (mutually exclusive with `needs`)
# needs = "other/task:finished"   # dependency (mutually exclusive with `schedule`)
# spawn = "other/child"           # enqueue children from `spawn_file`
# spawn_file = ".favetto/handoff.json"
# spawn_new_root = false          # each spawn child gets its own workflow root
# sign = "off"                    # "off" | "ssh" | "gpg"
# worktree = false                # read-only tasks: no worktree or branch

# [[vars]]
# name = "input_name"             # [a-zA-Z0-9_]+, unique, never `_prev`
# prompt = "Ask the user"
# default = "prefilled"
# required = false
# multiline = false
# type = "string"                 # "string" | "int" | "bool"
# choices = ["a", "b"]
---
You are an agent. Do the work described here and reply with a short summary.

Referenced inputs render with {{ input.input_name }}.
