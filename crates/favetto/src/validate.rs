//! `favetto validate`: parse a single task-definition file and report whether it
//! is a valid catalog entry.
//!
//! This is the offline counterpart of the daemon's catalog load. It calls the
//! same [`favetto_core::tasks::parse_task_md`] the live reload uses, so a file
//! that validates here parses identically when the daemon picks it up — no
//! second, drift-prone copy of the rules. Useful in an editor hook, a pre-commit
//! check, or an agent workflow that authors task files.

use std::io::Read;

use crate::cli::ValidateArgs;
use crate::tasks::{parse_task_md, TaskDef};

/// `favetto validate [FILE] [--name <NAME>]`: validate one task definition.
///
/// The file is read from `args.file`, or from stdin when no path is given. The
/// task identity defaults to the file stem (or `task` for stdin); pass `--name`
/// to check a dotted/relational identity. On success the command prints a
/// one-line summary and exits 0; on a parse or validation error it returns an
/// error so the process exits non-zero.
pub fn run(args: &ValidateArgs) -> anyhow::Result<()> {
    let (content, derived) = read_source(args)?;
    let name = args.name.clone().unwrap_or(derived);
    let def = parse_task_md(&name, &content)?;
    println!("{}", summary(&def));
    Ok(())
}

/// Read the task source: `--file` or stdin, plus the name derived from the path
/// (`task` for stdin, which has no path).
fn read_source(args: &ValidateArgs) -> anyhow::Result<(String, String)> {
    match &args.file {
        Some(path) => {
            let content = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
            Ok((content, task_name_from_path(path)))
        }
        None => {
            let mut content = String::new();
            std::io::stdin()
                .read_to_string(&mut content)
                .map_err(|e| anyhow::anyhow!("cannot read stdin: {e}"))?;
            Ok((content, "task".to_string()))
        }
    }
}

/// A task's identity from its file path: the file stem, or the last component
/// for a path with no stem (e.g. `.md`).
fn task_name_from_path(path: &std::path::Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "task".to_string())
}

/// The success line: identity plus the resolved agent or action, when set.
fn summary(def: &TaskDef) -> String {
    if let Some(action) = &def.action {
        return format!("ok: {} (action={})", def.name, action.id);
    }
    match &def.agent {
        Some(agent) => format!("ok: {} (agent={agent})", def.name),
        None => format!("ok: {}", def.name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp(name: &str, contents: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("favetto-validate-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join(name);
        std::fs::write(&path, contents).expect("write temp task");
        path
    }

    #[test]
    fn valid_task_reports_ok_with_agent() {
        let path = write_temp("good.md", "agent = \"opencode\"\n---\nDo the thing.\n");
        let args = ValidateArgs {
            file: Some(path),
            name: None,
        };
        // Directly exercise the parsing path; `run` prints, so check the derived
        // pieces rather than stdout.
        let (content, derived) = read_source(&args).expect("read source");
        assert_eq!(derived, "good");
        let def = parse_task_md(&derived, &content).expect("valid task");
        assert_eq!(def.agent.as_deref(), Some("opencode"));
        assert_eq!(summary(&def), "ok: good (agent=opencode)");
    }

    #[test]
    fn missing_separator_still_parses_as_headerless_task() {
        // No `---` line means the whole file is the TOML header and the prompt is
        // empty; an empty header parses, so this is *valid* but suspicious.
        let def = parse_task_md("x", "agent = \"opencode\"\n").expect("parses");
        assert!(def.prompt.is_empty());
    }

    #[test]
    fn invalid_toml_is_reported() {
        let err = parse_task_md("x", "agent = \n---\nbody").expect_err("invalid toml");
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn invalid_needs_is_rejected() {
        let err =
            parse_task_md("x", "needs = \"other:whenever\"\n---\nbody").expect_err("invalid needs");
        assert!(err.to_string().contains("needs"), "{err}");
    }

    #[test]
    fn reserved_and_duplicate_var_names_are_rejected() {
        let reserved = parse_task_md("x", "[[vars]]\nname = \"_prev\"\nprompt = \"p\"\n---\nbody")
            .expect_err("reserved var name");
        assert!(reserved.to_string().contains("reserved"), "{reserved}");

        let duplicate = parse_task_md(
            "x",
            "[[vars]]\nname = \"a\"\nprompt = \"p\"\n\n[[vars]]\nname = \"a\"\nprompt = \"q\"\n---\nbody",
        )
        .expect_err("duplicate var name");
        assert!(duplicate.to_string().contains("duplicate"), "{duplicate}");
    }

    #[test]
    fn action_task_reports_ok_with_action() {
        let def = parse_task_md(
            "gate",
            "kind = \"action\"\n[action]\nid = \"http.request\"\n---\n",
        )
        .expect("valid action task");
        assert_eq!(summary(&def), "ok: gate (action=http.request)");
    }

    #[test]
    fn action_task_without_action_is_rejected() {
        let err = parse_task_md("gate", "kind = \"action\"\n---\nbody")
            .expect_err("kind = action without [action]");
        assert!(err.to_string().contains("action"), "{err}");
    }

    #[test]
    fn action_task_with_agent_is_rejected() {
        let err = parse_task_md(
            "gate",
            "kind = \"action\"\nagent = \"opencode\"\n[action]\nid = \"http.request\"\n---\nbody",
        )
        .expect_err("kind = action with an agent");
        assert!(err.to_string().contains("must not set"), "{err}");
    }

    #[test]
    fn name_derives_from_the_file_stem() {
        assert_eq!(
            task_name_from_path(std::path::Path::new("/tmp/tasks/release/notes.md")),
            "notes"
        );
        assert_eq!(
            task_name_from_path(std::path::Path::new("stray.md")),
            "stray"
        );
    }
}
