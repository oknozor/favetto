//! Pure helpers for opening a catalog task in the user's editor.
//!
//! The TUI owns the terminal, so it resolves an editor command, runs it on a
//! client-local temp file, and reads the result back. Everything here is
//! side-effect free and unit-tested; the actual terminal suspend/restore and
//! process spawn live in [`super::mod`].

use std::path::PathBuf;

/// Split an editor command line into argv (ASCII whitespace; no shell quoting).
pub fn split_command(value: &str) -> Vec<String> {
    value.split_ascii_whitespace().map(str::to_string).collect()
}

/// Resolve the editor argv: config -> `$VISUAL` -> `$EDITOR` -> `vi`.
pub fn resolve_editor_command(
    config: Option<&str>,
    visual: Option<&str>,
    editor: Option<&str>,
) -> Vec<String> {
    for candidate in [config, visual, editor] {
        if let Some(value) = candidate.map(str::trim).filter(|v| !v.is_empty()) {
            let argv = split_command(value);
            if !argv.is_empty() {
                return argv;
            }
        }
    }
    vec!["vi".to_string()]
}

/// A unique temp path for editing `name`; `.md` so editors highlight Markdown.
pub fn temp_edit_path(name: &str) -> PathBuf {
    let stem = name.replace('/', "_");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "favetto-edit-{}-{nanos}-{stem}.md",
        std::process::id()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn split_command_splits_args() {
        assert_eq!(split_command("code --wait"), strings(&["code", "--wait"]));
        assert_eq!(split_command("  nvim   -f  "), strings(&["nvim", "-f"]));
        assert!(split_command("   ").is_empty());
    }

    #[test]
    fn resolve_prefers_config() {
        assert_eq!(
            resolve_editor_command(Some("code -w"), Some("vim"), Some("nano")),
            strings(&["code", "-w"])
        );
    }

    #[test]
    fn resolve_uses_visual_when_config_unset() {
        assert_eq!(
            resolve_editor_command(None, Some("nvim -f"), Some("nano")),
            strings(&["nvim", "-f"])
        );
    }

    #[test]
    fn resolve_uses_editor_when_visual_unset() {
        assert_eq!(
            resolve_editor_command(None, None, Some("emacs -nw")),
            strings(&["emacs", "-nw"])
        );
    }

    #[test]
    fn resolve_falls_back_to_vi() {
        assert_eq!(resolve_editor_command(None, None, None), strings(&["vi"]));
    }

    #[test]
    fn resolve_ignores_blank_values() {
        assert_eq!(
            resolve_editor_command(Some("  "), Some(""), Some("nano")),
            strings(&["nano"])
        );
    }

    #[test]
    fn temp_edit_path_uses_stem_and_md() {
        let path = temp_edit_path("pipelines/plan");
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("md"));
        let file = path.file_name().and_then(|n| n.to_str()).unwrap();
        assert!(file.contains("pipelines_plan"), "{file}");
        assert!(!file.contains('/'), "stem must be flattened: {file}");
    }
}
