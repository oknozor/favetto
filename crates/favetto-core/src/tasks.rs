//! Task catalog: task definitions are Markdown files with a TOML front-matter
//! header.
//!
//! ```md
//! agent = "opencode"                     # optional — external agent
//! provider = "jev"                       # optional — model provider
//! model = "1.13"                         # optional — model id (uses run_args)
//! cwd = "/path/to/repo"                  # optional — working directory
//! schedule = "0 0 * * * *"               # optional — makes this a recurring task
//! needs = "another_task:finished"        # optional — start when `another_task` ends (either outcome)
//! needs = "another_task:succeeded"       # optional — start only when `another_task` succeeds
//! needs = "another_task:failed"          # optional — start only when `another_task` fails
//! needs = "another_task:all_finished"    # optional — start once after every run in the root
//! spawn = "child_task"                   # optional — fan out from a handoff file
//! spawn_file = ".favetto/{{ task.id }}/manifest.json"  # array → one child per item
//! spawn_new_root = true                  # optional — each child starts its own workflow root
//! ---
//! You are an engineering agent…
//! ```
//!
//! Everything before the first `---` line is TOML; everything after is the prompt.
//! The prompt and `spawn_file` are rendered against a small context
//! (`{{ task.id }}`, `{{ input.* }}`, `{{ prev.output }}`) before use.
//!
//! The catalog is loaded **recursively** from a directory (`tasks/` by default):
//! `*.md` files may live at the root or in any subfolder. A task's identity is
//! its path relative to that root, without the `.md` extension, with `/`
//! separators (e.g. `pipelines/plan`); a root-level file keeps its bare stem
//! (`triage`). The same relative path is used by `catalog.get`, `tasks.start`,
//! `needs`/`spawn`, webhook rules, `catalog:<name>` schedule ids and
//! `{{ task.name }}`, so two files with the same stem in different folders are
//! distinct tasks.

use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

use crate::config::GitSigning;

// The pure value types moved to `favetto-wire`; keep the old paths resolving.
pub use favetto_wire::task_var::{TaskVar, VarType};

/// A task definition from a `*.md` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskDef {
    /// Path relative to the tasks root, without the `.md` extension,
    /// `/`-separated (e.g. `pipelines/plan`). A root-level file keeps its bare
    /// stem.
    pub name: String,
    /// Optional external agent name from `[agents.*]`. Overrides the global default.
    pub agent: Option<String>,
    /// Optional model provider (e.g. `"jev"`), substituted as `{provider}`.
    pub provider: Option<String>,
    /// Optional model id (e.g. `"1.13"`), substituted as `{model}`. When set, the
    /// agent's `run_args` template is used; otherwise `headless_args`.
    pub model: Option<String>,
    /// Optional working directory the agent runs in (e.g. a repo checkout).
    pub cwd: Option<String>,
    /// Optional cron expression; makes this a recurring task.
    pub schedule: Option<String>,
    /// Optional dependency. `"<task>:finished"` starts this task once per
    /// finished predecessor; `"<task>:all_finished"` is a root-scoped fan-in
    /// that starts it once after every `<task>` instance of the workflow root
    /// reaches a terminal state.
    pub needs: Option<String>,
    /// Optional catalog task to enqueue from this task's handoff file. When set,
    /// after a successful run the file at `spawn_file` is read as a JSON array and
    /// one `spawn` task is enqueued per element, with that element as its input.
    pub spawn: Option<String>,
    /// Path (relative to the task's working directory, or absolute) of the JSON
    /// handoff file consumed by `spawn`. Rendered as a template at run time.
    pub spawn_file: Option<String>,
    /// When true, each `spawn` child is enqueued as its own workflow root rather
    /// than as a descendant of this task. This lets a pipeline restart itself:
    /// the child's own `:all_finished` fan-in starts a fresh successor instead of
    /// being deduped against the root that already fired it.
    pub spawn_new_root: bool,
    /// Optional per-task override of the `[git] signing` mode for this run.
    pub sign: Option<GitSigning>,
    /// Optional per-task override of `[executor].worktree` isolation. When
    /// `false`, the task runs directly in its base directory (serialized per
    /// directory) and never creates a `favetto/*` branch or linked worktree.
    /// `None`/`true` inherits the global setting.
    pub worktree: Option<bool>,
    /// Manual input variables declared with `[[vars]]`; the TUI prompts for these
    /// before starting the task, and the executor enforces `required` ones.
    pub vars: Vec<TaskVar>,
    /// The Markdown prompt body.
    pub prompt: String,
}

#[derive(Debug, Deserialize)]
struct Header {
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    schedule: Option<String>,
    #[serde(default)]
    needs: Option<String>,
    #[serde(default)]
    spawn: Option<String>,
    #[serde(default)]
    spawn_file: Option<String>,
    #[serde(default)]
    spawn_new_root: bool,
    #[serde(default)]
    sign: Option<GitSigning>,
    #[serde(default)]
    worktree: Option<bool>,
    #[serde(default)]
    vars: Vec<TaskVar>,
}

/// Reject variable declarations that would produce an unusable or ambiguous
/// `input` object: empty/invalid names, the reserved `_prev`, duplicates, or an
/// empty prompt. Returning `Err` makes `reload_catalog` keep the previous
/// definition instead of dropping the task.
fn validate_vars(vars: &[TaskVar]) -> anyhow::Result<()> {
    let mut seen = std::collections::HashSet::new();
    for var in vars {
        if var.name.is_empty() {
            anyhow::bail!("input variable name must not be empty");
        }
        if var.name == "_prev" {
            anyhow::bail!("input variable name '_prev' is reserved");
        }
        if !var
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            anyhow::bail!(
                "invalid input variable name '{}': use [a-zA-Z0-9_]",
                var.name
            );
        }
        if !seen.insert(var.name.as_str()) {
            anyhow::bail!("duplicate input variable name '{}'", var.name);
        }
        if var.prompt.trim().is_empty() {
            anyhow::bail!("input variable '{}' needs a non-empty prompt", var.name);
        }
    }
    Ok(())
}

/// How a `needs` dependency reacts to its predecessor's `TaskFinished` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NeedsKind {
    /// Start once per finished predecessor instance, on either terminal outcome
    /// (`<task>:finished`, its alias `<task>:terminal`, or a bare `<task>`).
    Finished,
    /// Start once per successful predecessor instance (`<task>:succeeded`).
    Succeeded,
    /// Start once per failed predecessor instance (`<task>:failed`).
    Failed,
    /// Start once per workflow root, after every `<task>` instance of that root
    /// is terminal (`<task>:all_finished`).
    AllFinished,
}

/// Split a `needs` value into its source task and kind.
///
/// A bare value with no `<task>:<suffix>` shape keeps the legacy meaning: a
/// `:finished` dependency whose source is the whole value.
pub fn needs_parts(needs: &str) -> (&str, NeedsKind) {
    if let Some(source) = needs.strip_suffix(":all_finished") {
        (source, NeedsKind::AllFinished)
    } else if let Some(source) = needs.strip_suffix(":finished") {
        (source, NeedsKind::Finished)
    } else if let Some(source) = needs.strip_suffix(":terminal") {
        // `:terminal` is an explicit alias for `:finished`: either outcome.
        (source, NeedsKind::Finished)
    } else if let Some(source) = needs.strip_suffix(":succeeded") {
        (source, NeedsKind::Succeeded)
    } else if let Some(source) = needs.strip_suffix(":failed") {
        (source, NeedsKind::Failed)
    } else {
        (needs, NeedsKind::Finished)
    }
}

/// Validate a `needs` header value: either a bare legacy task name, or one of
/// `<task>:finished` / `<task>:terminal` / `<task>:succeeded` /
/// `<task>:failed` / `<task>:all_finished`. An invalid value makes the whole
/// file fail to parse, so `reload_catalog` keeps the previous definition.
fn validate_needs(needs: Option<&str>) -> anyhow::Result<()> {
    let Some(needs) = needs else {
        return Ok(());
    };
    if needs.trim().is_empty() {
        anyhow::bail!("`needs` must not be empty");
    }
    let (source, _) = needs_parts(needs);
    if source.is_empty() {
        anyhow::bail!("`needs` must name a task before its suffix");
    }
    const SUFFIXES: [&str; 5] = [
        ":finished",
        ":terminal",
        ":succeeded",
        ":failed",
        ":all_finished",
    ];
    if needs.contains(':') && !SUFFIXES.iter().any(|suffix| needs.ends_with(suffix)) {
        anyhow::bail!(
            "invalid `needs` value '{needs}': expected '<task>:finished', '<task>:terminal', \
             '<task>:succeeded', '<task>:failed' or '<task>:all_finished'"
        );
    }
    Ok(())
}

/// Parse a task definition from Markdown + TOML front-matter.
pub fn parse_task_md(name: &str, content: &str) -> anyhow::Result<TaskDef> {
    let mut header_lines: Vec<&str> = Vec::new();
    let mut body_start = None;

    for (i, line) in content.lines().enumerate() {
        if line.trim() == "---" {
            body_start = Some(i + 1);
            break;
        }
        header_lines.push(line);
    }

    let header: Header = toml::from_str(&header_lines.join("\n"))?;
    let prompt = match body_start {
        Some(i) => content.lines().skip(i).collect::<Vec<_>>().join("\n"),
        None => String::new(),
    };

    validate_vars(&header.vars)?;
    validate_needs(header.needs.as_deref())?;

    Ok(TaskDef {
        name: name.to_string(),
        agent: header.agent,
        provider: header.provider,
        model: header.model,
        cwd: header.cwd,
        schedule: header.schedule,
        needs: header.needs,
        spawn: header.spawn,
        spawn_file: header.spawn_file,
        spawn_new_root: header.spawn_new_root,
        sign: header.sign,
        worktree: header.worktree,
        vars: header.vars,
        prompt: prompt.trim().to_string(),
    })
}

/// Serialize a task definition back to Markdown.
pub fn to_markdown(def: &TaskDef) -> String {
    let mut out = String::new();
    if let Some(a) = &def.agent {
        out.push_str(&format!("agent = {a:?}\n"));
    }
    if let Some(p) = &def.provider {
        out.push_str(&format!("provider = {p:?}\n"));
    }
    if let Some(m) = &def.model {
        out.push_str(&format!("model = {m:?}\n"));
    }
    if let Some(c) = &def.cwd {
        out.push_str(&format!("cwd = {c:?}\n"));
    }
    if let Some(s) = &def.schedule {
        out.push_str(&format!("schedule = {s:?}\n"));
    }
    if let Some(n) = &def.needs {
        out.push_str(&format!("needs = {n:?}\n"));
    }
    if let Some(s) = &def.spawn {
        out.push_str(&format!("spawn = {s:?}\n"));
    }
    if let Some(s) = &def.spawn_file {
        out.push_str(&format!("spawn_file = {s:?}\n"));
    }
    if def.spawn_new_root {
        out.push_str("spawn_new_root = true\n");
    }
    if let Some(sign) = &def.sign {
        let sign = match sign {
            GitSigning::Off => "off",
            GitSigning::Gpg => "gpg",
            GitSigning::Ssh => "ssh",
        };
        out.push_str(&format!("sign = {sign:?}\n"));
    }
    // Only the `false` opt-out is meaningful to persist: `true`/absent inherit
    // the global `[executor].worktree`.
    if def.worktree == Some(false) {
        out.push_str("worktree = false\n");
    }
    // Array-of-tables must come after every scalar key, so `[[vars]]` is emitted
    // last in the header.
    for var in &def.vars {
        out.push_str("[[vars]]\n");
        out.push_str(&format!("name = {:?}\n", var.name));
        out.push_str(&format!("prompt = {:?}\n", var.prompt));
        if let Some(d) = &var.default {
            out.push_str(&format!("default = {d:?}\n"));
        }
        if var.required {
            out.push_str("required = true\n");
        }
        if var.multiline {
            out.push_str("multiline = true\n");
        }
        if var.var_type != VarType::String {
            let ty = match var.var_type {
                VarType::String => "string",
                VarType::Int => "int",
                VarType::Bool => "bool",
            };
            out.push_str(&format!("type = {ty:?}\n"));
        }
        if let Some(choices) = &var.choices {
            let rendered: Vec<String> = choices.iter().map(|c| format!("{c:?}")).collect();
            out.push_str(&format!("choices = [{}]\n", rendered.join(", ")));
        }
        out.push('\n');
    }
    out.push_str("---\n\n");
    out.push_str(&def.prompt);
    out
}

/// Load all `*.md` task definitions under `dir` (recursively). A missing
/// directory yields an empty catalog.
pub fn load_catalog(dir: &Path) -> anyhow::Result<Vec<TaskDef>> {
    Ok(reload_catalog(dir, &[]))
}

/// Reject a task name that would escape the tasks root or is otherwise unusable
/// as a relative task path. Used by `catalog.get` / `catalog.add` and
/// [`write_task_md`] (defense in depth: the RPC handlers are remotely reachable).
pub fn validate_task_path(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        anyhow::bail!("task name must not be empty");
    }
    if name.contains('\\') {
        anyhow::bail!("task name must use '/' separators");
    }
    let path = Path::new(name);
    if path.is_absolute() {
        anyhow::bail!("task name must be relative");
    }
    if path
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        anyhow::bail!("invalid task path '{name}'");
    }
    Ok(())
}

/// The path of `file` relative to `root`, `.md` stripped, `/`-separated.
fn relative_task_name(root: &Path, file: &Path) -> Option<String> {
    let rel = file.strip_prefix(root).ok()?;
    let rel = rel.with_extension(""); // drop `.md`
    let mut parts = Vec::new();
    for c in rel.components() {
        parts.push(c.as_os_str().to_str()?.to_string());
    }
    Some(parts.join("/"))
}

/// Collect `*.md` files under `dir`, recursively. Subdirectory read errors are
/// skipped; symlinked directories are not followed (`DirEntry::file_type` does
/// not resolve the link) so cycles cannot occur.
fn collect_task_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => collect_task_files(&path, out),
            _ => {
                if path.extension().and_then(|e| e.to_str()) == Some("md") {
                    out.push(path);
                }
            }
        }
    }
}

/// Reload the catalog under `dir`, merging the files on disk with `prior`.
///
/// Files that fail to parse (or read) keep their previous definition instead of
/// disappearing, so a half-written or momentarily invalid file never drops a task
/// from the live catalog. Files removed from disk are dropped and new files are
/// added. A directory that cannot be read (e.g. momentarily missing during an
/// atomic replace) keeps `prior` as-is. Discovery is recursive; each task's name
/// is its path relative to `dir` without the `.md` extension.
pub fn reload_catalog(dir: &Path, prior: &[TaskDef]) -> Vec<TaskDef> {
    if let Err(e) = std::fs::read_dir(dir) {
        if !prior.is_empty() || e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(dir = %dir.display(), error = %e, "failed to read task catalog directory");
        }
        return prior.to_vec();
    }

    let mut files = Vec::new();
    collect_task_files(dir, &mut files);

    let mut defs = Vec::new();
    for path in files {
        let Some(name) = relative_task_name(dir, &path) else {
            continue;
        };
        let previous = prior.iter().find(|d| d.name == name);

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to read task definition");
                if let Some(old) = previous {
                    defs.push(old.clone());
                }
                continue;
            }
        };
        match parse_task_md(&name, &content) {
            Ok(def) => defs.push(def),
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "skipping invalid task definition; keeping previous definition"
                );
                if let Some(old) = previous {
                    defs.push(old.clone());
                }
            }
        }
    }

    defs.sort_by(|a, b| a.name.cmp(&b.name));
    defs
}

/// Write a task definition to `<dir>/<name>.md`, creating parent folders.
pub fn write_task_md(dir: &Path, def: &TaskDef) -> anyhow::Result<()> {
    validate_task_path(&def.name)?;
    let path = dir.join(format!("{}.md", def.name));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, to_markdown(def))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn parses_agent_and_cwd_header() {
        let def = parse_task_md(
            "che",
            "agent = \"opencode\"\nprovider = \"jev\"\nmodel = \"1.13\"\ncwd = \"/code/che\"\n---\n\nDo the thing.\n",
        )
        .unwrap();
        assert_eq!(def.agent.as_deref(), Some("opencode"));
        assert_eq!(def.provider.as_deref(), Some("jev"));
        assert_eq!(def.model.as_deref(), Some("1.13"));
        assert_eq!(def.cwd.as_deref(), Some("/code/che"));
        assert_eq!(def.prompt, "Do the thing.");
    }

    #[test]
    fn parses_task_sign_header() {
        assert_eq!(
            parse_task_md("t", "sign = \"off\"\n---\nbody\n")
                .unwrap()
                .sign,
            Some(GitSigning::Off)
        );
        assert_eq!(
            parse_task_md("t", "sign = \"ssh\"\n---\nbody\n")
                .unwrap()
                .sign,
            Some(GitSigning::Ssh)
        );
        assert_eq!(
            parse_task_md("t", "sign = \"gpg\"\n---\nbody\n")
                .unwrap()
                .sign,
            Some(GitSigning::Gpg)
        );
        // No `sign` key -> no override.
        assert_eq!(
            parse_task_md("t", "agent = \"x\"\n---\nbody\n")
                .unwrap()
                .sign,
            None
        );
        // An invalid mode makes the whole file fail to parse, so `reload_catalog`
        // keeps the previous definition rather than silently changing behavior.
        assert!(parse_task_md("t", "sign = \"bogus\"\n---\nbody\n").is_err());
    }

    #[test]
    fn parses_task_worktree_header() {
        // Explicit opt-out parses and is the only value serialized.
        let def = parse_task_md("t", "worktree = false\n---\nbody\n").unwrap();
        assert_eq!(def.worktree, Some(false));
        let md = to_markdown(&def);
        assert!(md.contains("worktree = false"), "{md}");
        assert_eq!(parse_task_md("t", &md).unwrap().worktree, Some(false));

        // `true` parses but is not persisted (it is the inherited default).
        let def = parse_task_md("t", "worktree = true\n---\nbody\n").unwrap();
        assert_eq!(def.worktree, Some(true));
        assert!(!to_markdown(&def).contains("worktree"), "{def:?}");

        // Absent -> None.
        assert_eq!(
            parse_task_md("t", "agent = \"x\"\n---\nbody\n")
                .unwrap()
                .worktree,
            None
        );

        // A non-boolean value fails to parse.
        assert!(parse_task_md("t", "worktree = \"yes\"\n---\nbody\n").is_err());
    }

    #[test]
    fn parses_and_round_trips_all_finished_needs() {
        let def = parse_task_md(
            "join",
            "agent = \"x\"\nneeds = \"target:all_finished\"\n---\nbody\n",
        )
        .unwrap();
        assert_eq!(def.needs.as_deref(), Some("target:all_finished"));
        // The `needs` key round-trips verbatim.
        let md = to_markdown(&def);
        assert!(md.contains("needs = \"target:all_finished\""), "{md}");
        assert_eq!(parse_task_md("join", &md).unwrap().needs, def.needs);

        assert_eq!(
            needs_parts("target:all_finished"),
            ("target", NeedsKind::AllFinished)
        );
        assert_eq!(
            needs_parts("target:finished"),
            ("target", NeedsKind::Finished)
        );
        // A bare value is the legacy `:finished` form.
        assert_eq!(needs_parts("target"), ("target", NeedsKind::Finished));
    }

    #[test]
    fn parses_and_round_trips_outcome_needs() {
        for (suffix, kind) in [
            ("succeeded", NeedsKind::Succeeded),
            ("failed", NeedsKind::Failed),
            // `:terminal` is an explicit alias for `:finished`.
            ("terminal", NeedsKind::Finished),
        ] {
            let value = format!("target:{suffix}");
            let def = parse_task_md(
                "follower",
                &format!("agent = \"x\"\nneeds = {value:?}\n---\nbody\n"),
            )
            .unwrap_or_else(|e| panic!("`{value}` must parse: {e}"));
            assert_eq!(def.needs.as_deref(), Some(value.as_str()));
            // The `needs` key round-trips verbatim.
            let md = to_markdown(&def);
            assert!(md.contains(&format!("needs = {value:?}")), "{md}");
            assert_eq!(parse_task_md("follower", &md).unwrap().needs, def.needs);
            assert_eq!(needs_parts(&value), ("target", kind));
        }
    }

    #[test]
    fn rejects_unknown_needs_suffix() {
        let parse = |needs: &str| {
            parse_task_md(
                "t",
                &format!("agent = \"x\"\nneeds = {needs:?}\n---\nbody\n"),
            )
        };
        assert!(parse("target:finished").is_ok());
        assert!(parse("target:terminal").is_ok());
        assert!(parse("target:succeeded").is_ok());
        assert!(parse("target:failed").is_ok());
        assert!(parse("target:all_finished").is_ok());
        // Legacy bare names stay valid.
        assert!(parse("target").is_ok());
        // Unknown suffixes and empty sources are rejected.
        assert!(parse("target:bogus").is_err());
        assert!(parse("target:all_succeeded").is_err());
        assert!(parse(":finished").is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn parses_spawn_new_root_and_defaults_off() {
        // Absent -> false.
        let def = parse_task_md(
            "t",
            "agent = \"x\"\nspawn = \"child\"\nspawn_file = \"m.json\"\n---\nbody\n",
        )
        .unwrap();
        assert!(!def.spawn_new_root);

        // Present -> true, and it round-trips.
        let def = parse_task_md(
            "t",
            "agent = \"x\"\nspawn = \"child\"\nspawn_file = \"m.json\"\n\
             spawn_new_root = true\n---\nbody\n",
        )
        .unwrap();
        assert!(def.spawn_new_root);
        let md = to_markdown(&def);
        assert!(md.contains("spawn_new_root = true"), "{md}");
        assert!(parse_task_md("t", &md).unwrap().spawn_new_root);

        // A non-boolean value fails to parse.
        assert!(parse_task_md("t", "spawn_new_root = \"yes\"\n---\nbody\n").is_err());
    }

    #[test]
    fn round_trips_through_markdown() {
        let def = TaskDef {
            name: "t".to_string(),
            agent: Some("vibe".to_string()),
            provider: Some("jev".to_string()),
            model: Some("1.13".to_string()),
            cwd: Some("/tmp/repo".to_string()),
            schedule: None,
            needs: None,
            spawn: Some("plan".to_string()),
            spawn_file: Some(".favetto/{{ input.id }}/manifest.json".to_string()),
            spawn_new_root: true,
            sign: Some(GitSigning::Off),
            worktree: None,
            vars: vec![
                TaskVar {
                    name: "issue_description".to_string(),
                    prompt: "Describe the issue".to_string(),
                    default: None,
                    required: true,
                    multiline: true,
                    var_type: VarType::String,
                    choices: None,
                },
                TaskVar {
                    name: "count".to_string(),
                    prompt: "How many".to_string(),
                    default: Some("3".to_string()),
                    required: false,
                    multiline: false,
                    var_type: VarType::Int,
                    choices: None,
                },
                TaskVar {
                    name: "flavor".to_string(),
                    prompt: "Flavor".to_string(),
                    default: None,
                    required: false,
                    multiline: false,
                    var_type: VarType::String,
                    choices: Some(vec!["vanilla".to_string(), "mint".to_string()]),
                },
            ],
            prompt: "hello".to_string(),
        };
        let md = to_markdown(&def);
        let parsed = parse_task_md("t", &md).unwrap();
        assert_eq!(parsed.agent.as_deref(), Some("vibe"));
        assert_eq!(parsed.provider.as_deref(), Some("jev"));
        assert_eq!(parsed.model.as_deref(), Some("1.13"));
        assert_eq!(parsed.cwd.as_deref(), Some("/tmp/repo"));
        assert_eq!(parsed.spawn.as_deref(), Some("plan"));
        assert_eq!(
            parsed.spawn_file.as_deref(),
            Some(".favetto/{{ input.id }}/manifest.json")
        );
        assert!(parsed.spawn_new_root);
        assert_eq!(parsed.sign, Some(GitSigning::Off));
        assert_eq!(parsed.vars, def.vars);
        assert_eq!(parsed.prompt, "hello");
    }

    #[test]
    fn parses_vars_and_defaults_to_empty() {
        let def = parse_task_md(
            "issue",
            "agent = \"opencode\"\n\
             [[vars]]\n\
             name = \"issue_description\"\n\
             prompt = \"Describe the issue to file\"\n\
             multiline = true\n\
             required = true\n\
             [[vars]]\n\
             name = \"count\"\n\
             prompt = \"How many\"\n\
             default = \"3\"\n\
             type = \"int\"\n\
             [[vars]]\n\
             name = \"flavor\"\n\
             prompt = \"Flavor\"\n\
             choices = [\"vanilla\", \"mint\"]\n\
             ---\n\nfile it\n",
        )
        .unwrap();
        assert_eq!(def.vars.len(), 3);
        assert_eq!(def.vars[0].name, "issue_description");
        assert!(def.vars[0].required);
        assert!(def.vars[0].multiline);
        assert_eq!(def.vars[0].var_type, VarType::String);
        assert_eq!(def.vars[1].default.as_deref(), Some("3"));
        assert_eq!(def.vars[1].var_type, VarType::Int);
        assert_eq!(
            def.vars[2].choices.as_deref(),
            Some(["vanilla".to_string(), "mint".to_string()].as_slice())
        );

        // A file without `[[vars]]` parses with an empty list.
        let plain = parse_task_md("plain", "agent = \"x\"\n---\nbody\n").unwrap();
        assert!(plain.vars.is_empty());
    }

    #[test]
    fn rejects_invalid_vars() {
        let parse = |vars: &str| parse_task_md("t", &format!("agent = \"x\"\n{vars}---\nbody\n"));
        assert!(parse("[[vars]]\nname = \"bad name\"\nprompt = \"p\"\n").is_err());
        assert!(parse("[[vars]]\nname = \"_prev\"\nprompt = \"p\"\n").is_err());
        assert!(parse(
            "[[vars]]\nname = \"a\"\nprompt = \"p\"\n[[vars]]\nname = \"a\"\nprompt = \"q\"\n"
        )
        .is_err());
        assert!(parse("[[vars]]\nname = \"a\"\nprompt = \"\"\n").is_err());
        assert!(parse("[[vars]]\nname = \"\"\nprompt = \"p\"\n").is_err());
        assert!(parse("[[vars]]\nname = \"ok\"\nprompt = \"p\"\n").is_ok());
    }

    /// A unique scratch directory for catalog tests.
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "favetto-catalog-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reload_drops_deleted_files_and_adds_new_ones() {
        let dir = temp_dir("reload");
        std::fs::write(dir.join("a.md"), "agent = \"x\"\n---\nprompt a\n").unwrap();
        std::fs::write(dir.join("b.md"), "agent = \"x\"\n---\nprompt b\n").unwrap();
        let prior = load_catalog(&dir).unwrap();
        assert_eq!(prior.len(), 2);

        std::fs::remove_file(dir.join("b.md")).unwrap();
        std::fs::write(dir.join("c.md"), "agent = \"x\"\n---\nprompt c\n").unwrap();
        let reloaded = reload_catalog(&dir, &prior);
        let names: Vec<_> = reloaded.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["a", "c"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reload_keeps_previous_definition_on_parse_error() {
        let dir = temp_dir("parse-error");
        let good = "agent = \"x\"\n---\noriginal\n";
        std::fs::write(dir.join("a.md"), good).unwrap();
        let prior = load_catalog(&dir).unwrap();

        // A half-written file (invalid TOML header) must not drop the task.
        std::fs::write(dir.join("a.md"), "agent = \n---\nbroken\n").unwrap();
        let reloaded = reload_catalog(&dir, &prior);
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].name, "a");
        assert_eq!(reloaded[0].prompt, "original");

        // Once the file parses again the new definition wins.
        std::fs::write(dir.join("a.md"), "agent = \"x\"\n---\nupdated\n").unwrap();
        let reloaded = reload_catalog(&dir, &reloaded);
        assert_eq!(reloaded[0].prompt, "updated");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reload_missing_dir_keeps_prior_catalog() {
        let prior = vec![parse_task_md("a", "agent = \"x\"\n---\nprompt\n").unwrap()];
        let root = temp_dir("missing");
        let missing = root.join("does-not-exist");
        let reloaded = reload_catalog(&missing, &prior);
        assert_eq!(reloaded, prior);
        // With no prior, a missing directory is an empty catalog.
        assert!(reload_catalog(&missing, &[]).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Write a task file at `dir/<name>.md`, creating parent folders.
    fn write_nested(dir: &Path, name: &str, prompt: &str) {
        let path = dir.join(format!("{name}.md"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("agent = \"x\"\n---\n{prompt}\n")).unwrap();
    }

    #[test]
    fn loads_nested_tasks_with_relative_names() {
        let dir = temp_dir("nested");
        write_nested(&dir, "a", "root");
        write_nested(&dir, "pipelines/plan", "nested");
        let defs = load_catalog(&dir).unwrap();
        let names: Vec<_> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["a", "pipelines/plan"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn same_stem_in_two_folders_coexist() {
        let dir = temp_dir("same-stem");
        write_nested(&dir, "one/dup", "one");
        write_nested(&dir, "two/dup", "two");
        let defs = load_catalog(&dir).unwrap();
        let names: Vec<_> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["one/dup", "two/dup"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reload_drops_deleted_nested_file_and_keeps_invalid_one() {
        let dir = temp_dir("nested-reload");
        write_nested(&dir, "pipelines/a", "a");
        write_nested(&dir, "pipelines/b", "b");
        let prior = load_catalog(&dir).unwrap();

        std::fs::remove_file(dir.join("pipelines/a.md")).unwrap();
        // Break `b`'s header: the previous definition must survive.
        std::fs::write(dir.join("pipelines/b.md"), "agent = \n---\nbroken\n").unwrap();
        let reloaded = reload_catalog(&dir, &prior);
        let names: Vec<_> = reloaded.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["pipelines/b"]);
        assert_eq!(reloaded[0].prompt, "b");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_task_md_creates_subfolders_and_round_trips() {
        let dir = temp_dir("nested-write");
        let def = TaskDef {
            name: "pipelines/plan".to_string(),
            agent: Some("x".to_string()),
            provider: None,
            model: None,
            cwd: None,
            schedule: None,
            needs: None,
            spawn: None,
            spawn_file: None,
            spawn_new_root: false,
            sign: None,
            worktree: None,
            vars: Vec::new(),
            prompt: "nested body".to_string(),
        };
        write_task_md(&dir, &def).unwrap();
        assert!(dir.join("pipelines/plan.md").exists());

        let reloaded = load_catalog(&dir).unwrap();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].name, "pipelines/plan");
        assert_eq!(reloaded[0].prompt, "nested body");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_task_path_rejects_traversal() {
        assert!(validate_task_path("../x").is_err());
        assert!(validate_task_path("/abs").is_err());
        assert!(validate_task_path("a/../../b").is_err());
        assert!(validate_task_path("a\\b").is_err());
        assert!(validate_task_path("").is_err());
        assert!(validate_task_path(".").is_err());
        assert!(validate_task_path("pipelines/plan").is_ok());
        assert!(validate_task_path("plan").is_ok());
    }

    #[test]
    fn write_task_md_rejects_traversal() {
        let dir = temp_dir("write-traversal");
        let def = TaskDef {
            name: "../escape".to_string(),
            agent: None,
            provider: None,
            model: None,
            cwd: None,
            schedule: None,
            needs: None,
            spawn: None,
            spawn_file: None,
            spawn_new_root: false,
            sign: None,
            worktree: None,
            vars: Vec::new(),
            prompt: String::new(),
        };
        assert!(write_task_md(&dir, &def).is_err());
        assert!(!dir.join("..").join("escape.md").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn arb_utf8() -> impl Strategy<Value = String> {
        prop::collection::vec(any::<char>(), 0..200)
            .prop_map(|chars| chars.into_iter().collect::<String>())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn parse_task_md_never_panics_on_arbitrary_utf8(input in arb_utf8()) {
            let first = parse_task_md("proptest", &input);
            let second = parse_task_md("proptest", &input);
            // Parsing is deterministic: same input, same outcome.
            prop_assert_eq!(first.is_ok(), second.is_ok());
            if let (Ok(a), Ok(b)) = (&first, &second) {
                prop_assert_eq!(a, b);
            }
        }
    }

    /// Inputs that historically stress the frame/header split and the validators.
    #[test]
    fn parse_task_md_handles_adversarial_corpus() {
        for input in [
            "",
            "---",
            "---\n",
            "---\n---\n---\n",
            "\u{feff}---\nbody\n",
            "agent = \"x\"\n---",
            "needs = \":\"\n---\nbody\n",
            "needs = \"a:\"\n---\nbody\n",
            "needs = \"a:all_finished\"\n---\nbody\n",
            "[[vars]]\n",
            "[[vars]]\nname = \"\"\nprompt = \"p\"\n---\n",
            "agent = \"x\"\n---\n\u{0}\u{1}\u{2}\n",
        ] {
            let _ = parse_task_md("t", input); // must not panic
        }
    }
}
