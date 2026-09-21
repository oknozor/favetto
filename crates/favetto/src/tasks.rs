//! Task catalog: task definitions are Markdown files with a TOML front-matter
//! header.
//!
//! ```md
//! agent = "opencode"                     # optional — external agent
//! provider = "jev"                       # optional — model provider
//! model = "1.13"                         # optional — model id (uses run_args)
//! cwd = "/path/to/repo"                  # optional — working directory
//! schedule = "0 0 * * * *"               # optional — makes this a recurring task
//! needs = "another_task:finished"        # optional — start when `another_task` ends
//! spawn = "child_task"                   # optional — fan out from a handoff file
//! spawn_file = ".favetto/{{ task.id }}/manifest.json"  # array → one child per item
//! ---
//! You are an engineering agent…
//! ```
//!
//! Everything before the first `---` line is TOML; everything after is the prompt.
//! The prompt and `spawn_file` are rendered against a small context
//! (`{{ task.id }}`, `{{ input.* }}`, `{{ prev.output }}`) before use.
//! The catalog lives in a directory (`tasks/` by default) of `*.md` files.

use std::path::Path;

use serde::Deserialize;

/// A task definition from a `*.md` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskDef {
    /// File name without the `.md` extension.
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
    /// Optional dependency, e.g. `"another_task:finished"`.
    pub needs: Option<String>,
    /// Optional catalog task to enqueue from this task's handoff file. When set,
    /// after a successful run the file at `spawn_file` is read as a JSON array and
    /// one `spawn` task is enqueued per element, with that element as its input.
    pub spawn: Option<String>,
    /// Path (relative to the task's working directory, or absolute) of the JSON
    /// handoff file consumed by `spawn`. Rendered as a template at run time.
    pub spawn_file: Option<String>,
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
    out.push_str("---\n\n");
    out.push_str(&def.prompt);
    out
}

/// Load all `*.md` task definitions in `dir`. A missing directory yields an empty
/// catalog.
pub fn load_catalog(dir: &Path) -> anyhow::Result<Vec<TaskDef>> {
    Ok(reload_catalog(dir, &[]))
}

/// Reload the catalog in `dir`, merging the files on disk with `prior`.
///
/// Files that fail to parse (or read) keep their previous definition instead of
/// disappearing, so a half-written or momentarily invalid file never drops a task
/// from the live catalog. Files removed from disk are dropped and new files are
/// added. A directory that cannot be read (e.g. momentarily missing during an
/// atomic replace) keeps `prior` as-is.
pub fn reload_catalog(dir: &Path, prior: &[TaskDef]) -> Vec<TaskDef> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            if !prior.is_empty() || e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(dir = %dir.display(), error = %e, "failed to read task catalog directory");
            }
            return prior.to_vec();
        }
    };

    let mut defs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
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
        match parse_task_md(name, &content) {
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

/// Write a task definition to `<dir>/<name>.md`.
pub fn write_task_md(dir: &Path, def: &TaskDef) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.md", def.name));
    std::fs::write(path, to_markdown(def))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(parsed.prompt, "hello");
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
}
