# Agent skill

favetto ships a portable **agent skill** that teaches a coding agent to author
and validate task files:
[Favetto Task Authoring](https://github.com/oknozor/favetto/blob/main/skills/favetto-task-authoring/SKILL.md).
It is a plain `SKILL.md` directory plus references and a template, so it works
with any tool that understands the `SKILL.md` convention — OpenCode, Claude Code,
and other Agent Skills–compatible runners.

## What's in it

```text
skills/
├── index.json                          # HTTP-catalog manifest for remote install
└── favetto-task-authoring/
    ├── SKILL.md                        # when to use it + the workflow
    ├── references/
    │   ├── task-format.md              # header keys, [[vars]], identity
    │   ├── templating-and-graph.md     # {{ }} context, needs/spawn/workflow
    │   └── examples.md                 # one-shot, vars, needs, spawn, schedule
    └── assets/
        └── task-template.md            # skeleton to copy
```

The skill references [`favetto validate`](../reference/cli) for checking a task
without a running daemon.

## Install

Copy the skill directory into one of your agent's skill locations, or point the
tool at the repository copy.

**OpenCode** discovers skills in `.opencode/skills/`, `.agents/skills/`,
`.claude/skills/`, and `~/.config/opencode/skills/`. Copy the directory in:

```sh
cp -r skills/favetto-task-authoring .agents/skills/
```

Or add the repository (or its raw URL) to the `skills` array in
`opencode.jsonc`:

```jsonc
{
  "skills": ["./skills"],
}
```

**Claude Code** reads `.claude/skills/`; copy it there the same way:

```sh
cp -r skills/favetto-task-authoring .claude/skills/
```

**Remote install.** The repository root is an HTTP catalog: `skills/index.json`
lists the skill's files. Point a client at it by base URL, and it downloads each
file from `<base-url>/favetto-task-authoring/<file>`.

## Use

Once installed, the agent advertises the skill when a request is about writing a
favetto task, and loads it before editing the catalog. The same files are useful
by hand:
[`references/task-format.md`](https://github.com/oknozor/favetto/blob/main/skills/favetto-task-authoring/references/task-format.md)
is a compact restatement of the [task file format](../reference/tasks), and
[`assets/task-template.md`](https://github.com/oknozor/favetto/blob/main/skills/favetto-task-authoring/assets/task-template.md)
is a commented starting point you can copy into `tasks/`.

Validate any task you write or edit:

```sh
favetto validate tasks/release/notes.md
```

It prints `ok: <name>` and exits 0 for a valid file, or prints the parse error
and exits non-zero otherwise. This is the offline counterpart of the daemon's
catalog load and uses the same parser, so a file that validates here loads
identically.
