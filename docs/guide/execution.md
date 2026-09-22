# Parallel execution & git worktrees

The executor is serial by default. Parallelism and isolation are configurable:

```toml
[executor]
parallel = true          # default false
max_concurrency = 4
worktree = true          # default true; used only when `parallel`
# worktree_dir = "~/.local/share/favetto/worktrees"  # absolute or repo-relative
keep_worktree = true     # default true; false removes the worktree afterwards
```

- **Parallel + git repository** → each task runs in its own `git worktree`
  (branch `favetto/<task>-<id>`, path under the worktree dir), so tasks don't
  step on each other. Worktrees are kept by default so the agent's branch and
  changes can be inspected.
- **Parallel + not a repository** → tasks that share a working directory are
  serialized (one at a time); different directories still run concurrently.
- **Not parallel** → strict global serialization, as before.

`worktree` is ignored unless `parallel = true`. The effective directory is
`input.cwd` → task `cwd` → the daemon's working directory.
