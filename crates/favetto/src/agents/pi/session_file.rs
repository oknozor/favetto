//! Best-effort reader for pi's on-disk session store.
//!
//! pi persists each session as JSONL under
//! `~/.pi/agent/sessions/--<cwd-slug>--/<timestamp>_<session-id>.jsonl`. Only the
//! session display name (and header) are read; the format is private and may
//! change, so every helper is bounded and returns `None` rather than failing.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Longest file prefix scanned, so a huge session file is never fully read.
const MAX_BYTES: u64 = 256 * 1024;
/// Longest line considered, so one pathological line cannot dominate the scan.
const MAX_LINE_BYTES: usize = 64 * 1024;
/// Most lines scanned within [`MAX_BYTES`].
const MAX_LINES: usize = 512;

/// A parsed `session` header line.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) struct SessionHeader {
    pub id: String,
    pub cwd: Option<PathBuf>,
}

/// The pi session root: `$PI_HOME/agent/sessions`, else `~/.pi/agent/sessions`.
pub(crate) fn sessions_root() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("PI_HOME") {
        return Some(PathBuf::from(home).join("agent").join("sessions"));
    }
    dirs::home_dir().map(|home| home.join(".pi").join("agent").join("sessions"))
}

/// The directory component pi derives from a working directory: strip the
/// leading separator and replace `/`, `\` and `:` with `-`.
pub(crate) fn cwd_slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .trim_start_matches(['/', '\\'])
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':') {
                '-'
            } else {
                c
            }
        })
        .collect()
}

/// Parse a `session` header line.
#[allow(dead_code)]
pub(crate) fn parse_header(line: &str) -> Option<SessionHeader> {
    let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if value.get("type").and_then(|v| v.as_str()) != Some("session") {
        return None;
    }
    let id = value.get("id").and_then(|v| v.as_str())?.to_string();
    let cwd = value.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from);
    Some(SessionHeader { id, cwd })
}

/// Find the newest session file for `session_id` in `cwd`'s session directory.
pub(crate) fn find_session_file(cwd: &Path, session_id: &str) -> Option<PathBuf> {
    find_session_file_in(&sessions_root()?, cwd, session_id)
}

/// [`find_session_file`] against an explicit root, for tests.
pub(crate) fn find_session_file_in(root: &Path, cwd: &Path, session_id: &str) -> Option<PathBuf> {
    let suffix = format!("_{session_id}.jsonl");
    let mut newest: Option<(String, PathBuf)> = None;
    for dir in session_dirs(root, cwd) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(&suffix) {
                continue;
            }
            let better = newest.as_ref().is_none_or(|(best, _)| name > *best);
            if better {
                newest = Some((name, entry.path()));
            }
        }
    }
    newest.map(|(_, path)| path)
}

/// The candidate session directories for `cwd`: pi wraps the slug in `--`, but
/// older/newer versions may not, so both are probed.
pub(crate) fn session_dirs(root: &Path, cwd: &Path) -> [PathBuf; 2] {
    let slug = cwd_slug(cwd);
    [root.join(format!("--{slug}--")), root.join(&slug)]
}

/// The newest `.jsonl` session file for `cwd` whose modification time is at or
/// after `after` (when given). Used to discover an interactive session whose id
/// is not known before launch: the file pi creates for the new run wins by
/// mtime over any pre-existing session in the same directory.
pub(crate) fn newest_session_file_in(
    root: &Path,
    cwd: &Path,
    after: Option<SystemTime>,
) -> Option<PathBuf> {
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for dir in session_dirs(root, cwd) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            if let Some(after) = after {
                if modified < after {
                    continue;
                }
            }
            let better = newest.as_ref().is_none_or(|(best, _)| modified >= *best);
            if better {
                newest = Some((modified, path));
            }
        }
    }
    newest.map(|(_, path)| path)
}

/// Read the last non-empty `session_info.name` from a session file.
pub(crate) fn read_session_name(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut data = Vec::new();
    file.take(MAX_BYTES).read_to_end(&mut data).ok()?;
    let text = String::from_utf8_lossy(&data);
    let mut name = None;
    for line in text.lines().take(MAX_LINES) {
        if line.len() > MAX_LINE_BYTES {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        if value.get("type").and_then(|v| v.as_str()) != Some("session_info") {
            continue;
        }
        if let Some(found) = value.get("name").and_then(|v| v.as_str()) {
            let found = found.trim();
            if !found.is_empty() {
                name = Some(found.to_string());
            }
        }
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "favetto-pi-session-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn cwd_slug_normalizes_separators() {
        assert_eq!(cwd_slug(Path::new("/home/me/proj")), "home-me-proj");
        assert_eq!(cwd_slug(Path::new("\\home\\me")), "home-me");
        assert_eq!(cwd_slug(Path::new("C:\\Users\\me")), "C--Users-me");
    }

    #[test]
    fn parse_header_reads_id_and_cwd() {
        let header = parse_header(
            r#"{"type":"session","version":3,"id":"ses_1","timestamp":"2024-12-03T14:00:00.000Z","cwd":"/p"}"#,
        )
        .unwrap();
        assert_eq!(header.id, "ses_1");
        assert_eq!(header.cwd.as_deref(), Some(Path::new("/p")));

        assert!(parse_header(r#"{"type":"message","id":"x"}"#).is_none());
        assert!(parse_header("not json").is_none());
        assert!(parse_header(r#"{"type":"session"}"#).is_none());
    }

    #[test]
    fn find_session_file_picks_the_newest_match() {
        let root = temp_dir("find");
        let cwd = Path::new("/home/me/proj");
        let dir = root.join(format!("--{}--", cwd_slug(cwd)));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("111_ses_a.jsonl"), "").unwrap();
        std::fs::write(dir.join("222_ses_a.jsonl"), "").unwrap();
        std::fs::write(dir.join("333_other.jsonl"), "").unwrap();

        let found = find_session_file_in(&root, cwd, "ses_a").unwrap();
        assert_eq!(found.file_name().unwrap(), "222_ses_a.jsonl");
        assert!(find_session_file_in(&root, cwd, "missing").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn find_session_file_tolerates_the_unwrapped_dir() {
        let root = temp_dir("unwrap");
        let cwd = Path::new("/home/me/proj");
        let dir = root.join(cwd_slug(cwd));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("111_ses_a.jsonl"), "").unwrap();

        let found = find_session_file_in(&root, cwd, "ses_a").unwrap();
        assert_eq!(found.file_name().unwrap(), "111_ses_a.jsonl");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn read_session_name_keeps_the_last_non_empty_name() {
        let root = temp_dir("name");
        let path = root.join("s.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"session","version":3,"id":"ses_1"}"#,
                "\n",
                r#"{"type":"session_info","id":"a","name":"First"}"#,
                "\n",
                r#"{"type":"session_info","id":"b","name":"   "}"#,
                "\n",
                r#"{"type":"session_info","id":"c","name":"Refactor auth"}"#,
                "\n",
            ),
        )
        .unwrap();
        assert_eq!(read_session_name(&path).as_deref(), Some("Refactor auth"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn read_session_name_is_none_without_a_session_info() {
        let root = temp_dir("noname");
        let path = root.join("s.jsonl");
        std::fs::write(&path, r#"{"type":"session","id":"ses_1"}"#).unwrap();
        assert!(read_session_name(&path).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
