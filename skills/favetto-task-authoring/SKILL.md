---
name: Favetto Task Authoring
description: Author, edit, and validate favetto task-definition files (Markdown with a TOML header and a prompt body). Use when asked to create or edit a favetto task, a task catalog entry, a `tasks/**.md` file, a `[[vars]]` form, a scheduled/`needs`/`spawn` pipeline step, or to fix a task that fails to parse.
license: MIT
compatibility: Requires the `favetto` CLI for validation; otherwise plain files.
---

# Favetto Task Authoring

A **favetto task** is a Markdown file under the tasks directory (default
`tasks/`). Everything up to the first `---` line is a TOML header; everything
after it is the prompt. There is no other schema — create, edit, and remove
tasks by writing files on disk.

This skill is self-contained: follow it in any repository that uses favetto.
Read the references only when you need the detail.

## Workflow

1. **Read the references for the exact rules.**
   - [`references/task-format.md`](references/task-format.md) — header keys,
     `[[vars]]`, task identity, parsing rules.
   - [`references/templating-and-graph.md`](references/templating-and-graph.md) —
     the `{{ }}` prompt context and the `needs`/`spawn`/schedule graph.
   - [`references/examples.md`](references/examples.md) — ready-made examples.
2. **Copy [`assets/task-template.md`](assets/task-template.md)** as the file
   skeleton, or start from the closest example.
3. **Write the header, then the prompt body.** Keep the `---` separator on its
   own line. Choose the intent:
   - run once on demand → no `schedule`/`needs`;
   - recurring → `schedule = "<cron>"`;
   - follows another task → `needs = "<task>:<kind>"`;
   - enqueues children → `spawn` + `spawn_file`;
   - needs human input → declare `[[vars]]`.
4. **Place the file** under the tasks root. Its identity is the path relative to
   the root without `.md`, `/`-separated (for example `pipeline/plan`). Moving a
   file changes its identity and breaks any `needs`/`spawn`/schedule that named
   it.
5. **Validate before finishing:**

   ```sh
   favetto validate path/to/task.md
   ```

   It exits 0 and prints `ok: <name>` on success, and exits non-zero printing the
   parse error otherwise. Add `--name <identity>` to validate under a specific
   catalog identity. Without the CLI installed, the daemon is the fallback
   authority: a malformed file is skipped with a `skipping invalid task
   definition` warning and keeps its previous definition.

## Rules that matter most

- The header is **parsed as TOML until the first `---`**; the body is the prompt.
  A missing `---` leaves the prompt empty — always include the separator.
- `schedule` and `needs` are **mutually exclusive**.
- `spawn` requires `spawn_file`; the file must contain a JSON array (one child
  per element), and nothing else.
- A `[[vars]]` `name` must match `[a-zA-Z0-9_]+`, be unique in the file, be
  non-empty, and never be `_prev`; its `prompt` must be non-empty.
- Templates use `{{ dotted.path }}` over `task.*`, `input.*`, and `prev.*`.
  Objects/arrays render as compact JSON; missing paths render empty.
- Set `worktree = false` on read-only tasks (triage, review, planning) so they do
  not create an isolated worktree.
- Unknown header keys are silently ignored, but invalid *values* (bad TOML, a bad
  `needs` suffix, a malformed `[[vars]]`) make the whole file fail to parse.
