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
//! ---
//! You are an engineering agent…
//! ```
//!
//! Everything before the first `---` line is TOML; everything after is the prompt.
//! The catalog lives in a directory (`tasks/` by default) of `*.md` files.

use std::path::Path;

use serde::Deserialize;

/// A task definition from a `*.md` file.
#[derive(Debug, Clone)]
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
    out.push_str("---\n\n");
    out.push_str(&def.prompt);
    out
}

/// Load all `*.md` task definitions in `dir`. A missing directory yields an empty
/// catalog.
pub fn load_catalog(dir: &Path) -> anyhow::Result<Vec<TaskDef>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(Vec::new()),
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
        let content = std::fs::read_to_string(&path)?;
        match parse_task_md(name, &content) {
            Ok(def) => defs.push(def),
            Err(e) => tracing::warn!(path = %path.display(), error = %e, "skipping invalid task definition"),
        }
    }

    defs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(defs)
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
            prompt: "hello".to_string(),
        };
        let md = to_markdown(&def);
        let parsed = parse_task_md("t", &md).unwrap();
        assert_eq!(parsed.agent.as_deref(), Some("vibe"));
        assert_eq!(parsed.provider.as_deref(), Some("jev"));
        assert_eq!(parsed.model.as_deref(), Some("1.13"));
        assert_eq!(parsed.cwd.as_deref(), Some("/tmp/repo"));
        assert_eq!(parsed.prompt, "hello");
    }
}
