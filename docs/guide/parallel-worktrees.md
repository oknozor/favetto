# Parallel execution and worktrees

The executor is serial by default: one task runs at a time. When tasks are
independent — a triage fan-out, a batch of issue plans — turn on parallelism and
give each run its own git worktree so they cannot step on each other.

```toml
# ~/.config/favetto/config.toml
[executor]
parallel = true          # default false
max_concurrency = 4
worktree = true          # default true; used only when `parallel`
# worktree_dir = "~/.local/share/favetto/worktrees"  # absolute or repo-relative
keep_worktree = false    # default false; true keeps the worktree for inspection

# Retained worktrees are still reclaimed after `days`; active tasks are never
# touched and `days = 0` keeps opt-in worktrees forever.
[executor.worktree_retention]
days = 30
min_worktrees = 0
```

What the modes do:

- **Parallel + git repository** → each task runs in its own `git worktree`
  (branch `favetto/<task>-<id>`, path under the worktree dir), so tasks do not
  step on each other. By default the worktree and its branch are removed when the
  task finishes; set `keep_worktree = true` to inspect the agent's branch and
  changes afterwards.
- **Parallel + not a repository** → tasks that share a working directory are
  serialized (one at a time); tasks in different directories still run
  concurrently.
- **Not parallel** → strict global serialization, as before.

`worktree` is ignored unless `parallel = true`. The effective working directory,
most specific first, is `input.cwd` → task `cwd` → the agent's `cwd` → the
daemon's working directory.

`worktree_dir` expands a leading `~` to your home directory; an absolute path is
used as-is and a non-`~` relative path is resolved against the repo root.
`[daemon].data_dir` and `[daemon].tasks_dir` expand `~` the same way.

::: tip Inspect a running or kept run
With the default `keep_worktree = false` a finished task cleans up after itself,
so `git worktree list` shows worktrees only while tasks are running. To review a
finished run's branch, set `[executor].keep_worktree = true` (the branch
`favetto/<task>-<id>` is kept, and the retention sweep reclaims it after
`worktree_retention.days`):

```bash
git worktree list
git log favetto/plan-<id>
```
:::

Next: [Git and commit signing](./git-signing).
