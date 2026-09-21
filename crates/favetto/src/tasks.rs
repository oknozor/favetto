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

/// The declared type of a manual input variable. `int`/`bool` values are coerced
/// to JSON numbers/bools before being stored in the task's `input`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VarType {
    #[default]
    String,
    Int,
    Bool,
}

/// A manual input variable declared in a task's `[[vars]]` front-matter. The
/// collected value becomes `input.<name>` and renders through `{{ input.<name> }}`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
pub struct TaskVar {
    /// The `input` key. `[a-zA-Z0-9_]+`, unique within the file, never `_prev`.
    pub name: String,
    /// The label/question shown in the TUI prompt popup.
    pub prompt: String,
    /// Optional value pre-filled in the popup.
    #[serde(default)]
    pub default: Option<String>,
    /// When true, submission is blocked while the field is empty.
    #[serde(default)]
    pub required: bool,
    /// When true, `Enter` inserts a newline and the submit chord completes the form.
    #[serde(default)]
    pub multiline: bool,
    /// The value type; `int`/`bool` are parsed before being stored.
    #[serde(default, rename = "type")]
    pub var_type: VarType,
    /// Optional fixed list of options, rendered as a selectable list.
    #[serde(default)]
    pub choices: Option<Vec<String>>,
}

impl TaskVar {
    /// Coerce a raw string entered by the user into the var's JSON representation.
    pub fn coerce(&self, raw: &str) -> anyhow::Result<serde_json::Value> {
        if let Some(choices) = &self.choices {
            if !choices.iter().any(|c| c == raw) {
                anyhow::bail!("must be one of: {}", choices.join(", "));
            }
        }
        match self.var_type {
            VarType::String => Ok(serde_json::Value::String(raw.to_string())),
            VarType::Int => {
                let n = raw
                    .trim()
                    .parse::<i64>()
                    .map_err(|_| anyhow::anyhow!("must be an integer"))?;
                Ok(serde_json::Value::Number(n.into()))
            }
            VarType::Bool => match raw.trim().to_ascii_lowercase().as_str() {
                "true" | "1" => Ok(serde_json::Value::Bool(true)),
                "false" | "0" => Ok(serde_json::Value::Bool(false)),
                _ => anyhow::bail!("must be true or false"),
            },
        }
    }
}

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

    #[test]
    fn coerces_typed_values() {
        let var = |var_type, choices| TaskVar {
            name: "v".to_string(),
            prompt: "p".to_string(),
            default: None,
            required: false,
            multiline: false,
            var_type,
            choices,
        };
        assert_eq!(
            var(VarType::String, None).coerce("hi").unwrap(),
            serde_json::json!("hi")
        );
        assert_eq!(
            var(VarType::Int, None).coerce(" 3 ").unwrap(),
            serde_json::json!(3)
        );
        assert!(var(VarType::Int, None).coerce("not an int").is_err());
        assert_eq!(
            var(VarType::Bool, None).coerce("True").unwrap(),
            serde_json::json!(true)
        );
        assert_eq!(
            var(VarType::Bool, None).coerce("0").unwrap(),
            serde_json::json!(false)
        );
        assert!(var(VarType::Bool, None).coerce("maybe").is_err());

        let choices = Some(vec!["a".to_string(), "b".to_string()]);
        assert!(var(VarType::String, choices.clone()).coerce("a").is_ok());
        assert!(var(VarType::String, choices).coerce("c").is_err());
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
