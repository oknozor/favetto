# Catalog

The catalog is the set of task files the daemon knows about. You add, edit, and
remove files on disk; the running daemon picks the changes up automatically. This
page is about *organising and finding* tasks — for the file syntax see the
[task file format reference](../reference/tasks).

## Organise tasks into folders

Put task files anywhere under the tasks directory (default `tasks/`, set with
`--tasks-dir` or `[daemon].tasks_dir`). The daemon loads them **recursively**:

```text
tasks/
  triage.md              →  task "triage"
  release/
    changelog.md         →  task "release/changelog"
    publish.md           →  task "release/publish"
```

A task's **identity** is its path relative to the tasks root, without the `.md`
extension, with `/` separators. Use that identity everywhere a task is named:

- starting a task from the TUI or `tasks.start`,
- `catalog.get` for the preview,
- `needs` and `spawn`,
- webhook `rule.task`,
- `catalog:<name>` schedule ids,
- <span v-pre>`{{ task.name }}`</span> in a prompt.

Because the identity is the relative path, two files named `plan.md` in different
folders are distinct tasks.

::: warning Renaming is a new task
Moving `triage.md` to `pipeline/triage.md` changes its identity from `triage` to
`pipeline/triage`. The old `catalog:triage` schedule is dropped and recreated,
and any `needs`/`spawn`/webhook rule that named `triage` must be updated.
:::

## Browse and start tasks in the TUI

The **Catalog** tab renders the catalog as a collapsible tree. Each subfolder is
a folder row (`📂` expanded, `📁` collapsed) and each task is a file row (`📄`),
indented by depth.

- `↑`/`↓` move the selection and load the preview for the new task.
- `Space`, or a click on a folder row, folds or unfolds it.
- `Enter` on a folder only folds/unfolds — it never starts a task.
- `Enter` on a task starts it; a task that declares `[[vars]]` opens its form
  first. Clicking a task twice does the same.

Collapse state lasts for the session and survives live reloads. On terminals
where the emoji render double-width, set `FAVETTO_PLAIN_ICONS=1` for an ASCII
fallback (`[-]`/`[+]`/`-`).

## Preview a task

The right side of the Catalog tab previews the highlighted task: its raw `.md`
source with the TOML header highlighted and the prompt rendered as Markdown
(headings, lists, blockquotes, fenced code, inline formatting, and links).
Moving the selection with `↑`/`↓` updates the preview; selecting a folder row
clears it. Scroll the preview with `PageUp`/`PageDown` or the mouse wheel.

## Live reload

The daemon watches the tasks directory and its subfolders (`notify`, 200 ms
debounce). Adding, editing, or removing a `.md` file reloads the catalog on the
fly and pushes `catalog.updated` so attached TUIs re-fetch the list and refresh
the preview.

```text
# in another terminal, while the daemon runs
echo 'agent = "opencode"
---
Summarise the open issues.' > tasks/summarise.md
# → the new task appears in the Catalog tab immediately
```

Two safety behaviours make editing calm:

- A file that fails to parse **keeps its previous definition**, so a half-written
  edit never drops a task. Fix the file and the new definition takes over.
- Recurring tasks derived from `schedule` headers are re-synced: a changed cron
  is re-registered, a removed header deletes its `catalog:<name>` schedule, and
  manually created schedules are left alone.

Runs already in flight keep the definition they were dispatched with; a queued
task picks up the freshly loaded definition when it starts.

## Add a task from the TUI

The Ctrl+P menu's **Add task to catalog** writes a new `.md` file (it does not
run it). The name may contain `/` to place the file in a subfolder — for example
`release/notes` creates `tasks/release/notes.md`. Files written this way appear
in the catalog through the same live reload described above.

Next: [Variables and prompts](./variables-and-prompts).
