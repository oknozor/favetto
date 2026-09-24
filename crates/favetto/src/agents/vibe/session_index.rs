//! Best-effort reader for Vibe's on-disk session store.
//!
//! Vibe keeps one directory per session under `$VIBE_HOME/logs/session`
//! (`~/.vibe` by default):
//!
//! ```text
//! logs/session/
//! ├── .session_index.json          # cache: dir name -> { session_id, cwd, title, … }
//! └── session_<ts>_<short-id>/
//!     ├── meta.json                # source of truth (id, cwd, title, stats, …)
//!     └── messages.jsonl
//! ```
//!
//! `--output streaming` now carries `sessionId` on every row, but the stream
//! still has no title and no token/cost usage; both live in this store. The
//! format is private and versioned, so every helper is bounded and returns
//! `None` rather than failing.

use std::io::Read;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use favetto_core::model::AgentUsage;

/// The persisted index cache inside the session directory.
const INDEX_FILENAME: &str = ".session_index.json";
/// The per-session metadata file (the source of truth).
const METADATA_FILENAME: &str = "meta.json";
/// Longest file prefix read, so a huge meta file is never fully loaded.
const MAX_BYTES: u64 = 256 * 1024;
/// Only sessions started within this window of the launch are candidates.
const RECENT_SLACK_SECONDS: i64 = 10;

/// A session's id and (optional) human-readable title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionIdentity {
    pub id: String,
    pub title: Option<String>,
}

/// One parsed `.session_index.json` entry.
#[derive(Debug, Clone)]
struct IndexEntry {
    /// The session directory name (`session_<ts>_<short-id>`).
    key: String,
    id: String,
    cwd: Option<String>,
    start_time: Option<DateTime<Utc>>,
    mtime_ns: u64,
    title: Option<String>,
}

/// Vibe's home directory: `$VIBE_HOME` when set (a test hook), else `~/.vibe`.
pub(crate) fn home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("VIBE_HOME") {
        return Some(PathBuf::from(home));
    }
    dirs::home_dir().map(|home| home.join(".vibe"))
}

/// The session directory inside a Vibe home: `<root>/logs/session`.
fn sessions_dir(root: &Path) -> PathBuf {
    root.join("logs").join("session")
}

/// The index path inside a Vibe home.
fn index_path(root: &Path) -> PathBuf {
    sessions_dir(root).join(INDEX_FILENAME)
}

/// Read a bounded JSON file, or `None` on any I/O or parse error.
fn read_json_bounded(path: &Path) -> Option<serde_json::Value> {
    let file = std::fs::File::open(path).ok()?;
    let mut data = Vec::new();
    file.take(MAX_BYTES).read_to_end(&mut data).ok()?;
    serde_json::from_slice(&data).ok()
}

/// Parse a non-empty JSON string field.
fn string_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Parse an RFC 3339 timestamp, normalising it to UTC.
fn parse_time(value: &serde_json::Value, key: &str) -> Option<DateTime<Utc>> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
}

/// Parse `.session_index.json` into entries, skipping malformed records.
fn parse_index(root: &Path) -> Vec<IndexEntry> {
    let Some(value) = read_json_bounded(&index_path(root)) else {
        return Vec::new();
    };
    let Some(object) = value.as_object() else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for (key, record) in object {
        let Some(id) = string_field(record, "session_id") else {
            continue;
        };
        entries.push(IndexEntry {
            key: key.clone(),
            id,
            cwd: string_field(record, "cwd"),
            start_time: parse_time(record, "start_time"),
            mtime_ns: record.get("mtime_ns").and_then(|v| v.as_u64()).unwrap_or(0),
            title: string_field(record, "title"),
        });
    }
    entries
}

/// Read the `title` from a session directory's `meta.json`.
fn read_meta_title(sessions: &Path, key: &str) -> Option<String> {
    let meta = read_json_bounded(&sessions.join(key).join(METADATA_FILENAME))?;
    string_field(&meta, "title")
}

/// Find the session directory whose `meta.json` carries `session_id`.
fn find_meta_dir(sessions: &Path, session_id: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(sessions).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("session_") {
            continue;
        }
        let dir = entry.path();
        let Some(meta) = read_json_bounded(&dir.join(METADATA_FILENAME)) else {
            continue;
        };
        if meta.get("session_id").and_then(|v| v.as_str()) == Some(session_id) {
            return Some(dir);
        }
    }
    None
}

/// The session directory for `session_id`: the index key when known, else a
/// metadata scan.
fn session_dir(root: &Path, session_id: &str) -> Option<PathBuf> {
    let sessions = sessions_dir(root);
    if let Some(entry) = parse_index(root).into_iter().find(|e| e.id == session_id) {
        return Some(sessions.join(entry.key));
    }
    find_meta_dir(&sessions, session_id)
}

/// Find the most recent session started in `cwd` at or after `since` (with a
/// small clock skew allowance), returning its id and title.
///
/// A session whose `start_time` is missing is not a candidate: the key is
/// `cwd` + start time, and a missing time would make the match ambiguous.
pub(crate) fn find_recent_session(
    root: &Path,
    cwd: &Path,
    since: DateTime<Utc>,
) -> Option<SessionIdentity> {
    let sessions = sessions_dir(root);
    let cutoff = since - chrono::Duration::seconds(RECENT_SLACK_SECONDS);
    parse_index(root)
        .into_iter()
        .filter(|entry| {
            entry
                .cwd
                .as_deref()
                .is_some_and(|value| Path::new(value) == cwd)
        })
        .filter(|entry| entry.start_time.is_some_and(|time| time >= cutoff))
        .max_by_key(|entry| (entry.start_time, entry.mtime_ns))
        .map(|entry| SessionIdentity {
            id: entry.id,
            title: entry
                .title
                .clone()
                .or_else(|| read_meta_title(&sessions, &entry.key)),
        })
}

/// Find a session anywhere in the store by its id, using [`home`] as the root.
pub(crate) fn find_session(session_id: &str) -> Option<SessionIdentity> {
    find_session_in(&home()?, session_id)
}

/// [`find_session`] against an explicit root, for the live parser and tests.
pub(crate) fn find_session_in(root: &Path, session_id: &str) -> Option<SessionIdentity> {
    let sessions = sessions_dir(root);
    if let Some(entry) = parse_index(root).into_iter().find(|e| e.id == session_id) {
        return Some(SessionIdentity {
            id: entry.id,
            title: entry
                .title
                .clone()
                .or_else(|| read_meta_title(&sessions, &entry.key)),
        });
    }
    // The cache can lag a fresh session; fall back to scanning metadata.
    let dir = find_meta_dir(&sessions, session_id)?;
    let meta = read_json_bounded(&dir.join(METADATA_FILENAME))?;
    Some(SessionIdentity {
        id: session_id.to_string(),
        title: string_field(&meta, "title"),
    })
}

/// Read the token/cost totals a session recorded in its `meta.json`.
///
/// Vibe's stream no longer emits a usage row, so this backfills the summary on
/// a headless run. Returns `None` when the session or its `stats` are missing.
pub(crate) fn read_meta_usage(root: &Path, session_id: &str) -> Option<AgentUsage> {
    let dir = session_dir(root, session_id)?;
    let meta = read_json_bounded(&dir.join(METADATA_FILENAME))?;
    let stats = meta.get("stats")?;
    Some(AgentUsage {
        input_tokens: u64_field(stats, "session_prompt_tokens"),
        output_tokens: u64_field(stats, "session_completion_tokens"),
        reasoning_tokens: 0,
        cache_read_tokens: u64_field(stats, "session_cached_tokens"),
        cache_write_tokens: 0,
        cost_usd: stats.get("session_cost").and_then(|v| v.as_f64()),
    })
}

/// Read a non-negative integer field, defaulting to `0`.
fn u64_field(value: &serde_json::Value, key: &str) -> u64 {
    value.get(key).and_then(|v| v.as_u64()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "favetto-vibe-index-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("logs").join("session")).unwrap();
        dir
    }

    fn write_index(root: &Path, body: &serde_json::Value) {
        std::fs::write(index_path(root), serde_json::to_vec(body).unwrap()).unwrap();
    }

    fn session_dir(root: &Path, key: &str) -> PathBuf {
        let dir = sessions_dir(root).join(key);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn find_recent_session_picks_the_matching_cwd_and_time() {
        let root = temp_dir("recent");
        let cwd = Path::new("/home/me/proj");
        let since = DateTime::parse_from_rfc3339("2026-09-24T10:00:00+00:00")
            .unwrap()
            .with_timezone(&Utc);
        write_index(
            &root,
            &serde_json::json!({
                "session_20260924_100000_aaaa": {
                    "session_id": "ses_old",
                    "cwd": "/home/me/proj",
                    "start_time": "2026-09-24T09:00:00+00:00",
                    "mtime_ns": 1_i64,
                    "title": "Old"
                },
                "session_20260924_100500_bbbb": {
                    "session_id": "ses_new",
                    "cwd": "/home/me/proj",
                    "start_time": "2026-09-24T10:00:05+00:00",
                    "mtime_ns": 2_i64,
                    "title": "New"
                },
                "session_20260924_100600_cccc": {
                    "session_id": "ses_other_cwd",
                    "cwd": "/elsewhere",
                    "start_time": "2026-09-24T10:00:06+00:00",
                    "mtime_ns": 3_i64,
                    "title": "Other"
                }
            }),
        );
        let found = find_recent_session(&root, cwd, since).unwrap();
        assert_eq!(found.id, "ses_new");
        assert_eq!(found.title.as_deref(), Some("New"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn find_recent_session_without_entries_returns_none() {
        let root = temp_dir("empty");
        let cwd = Path::new("/home/me/proj");
        let since = Utc::now();
        assert!(find_recent_session(&root, cwd, since).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn find_session_in_reads_title_from_meta_when_index_title_is_null() {
        let root = temp_dir("title");
        let key = "session_20260924_100000_dddd";
        write_index(
            &root,
            &serde_json::json!({
                key: {
                    "session_id": "ses_title",
                    "cwd": "/p",
                    "start_time": "2026-09-24T10:00:00+00:00",
                    "mtime_ns": 1_i64,
                    "title": null
                }
            }),
        );
        std::fs::write(
            session_dir(&root, key).join(METADATA_FILENAME),
            r#"{"session_id":"ses_title","title":"Fix the bug"}"#,
        )
        .unwrap();

        let found = find_session_in(&root, "ses_title").unwrap();
        assert_eq!(found.id, "ses_title");
        assert_eq!(found.title.as_deref(), Some("Fix the bug"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn find_session_in_falls_back_to_meta_scan() {
        let root = temp_dir("scan");
        // Index is empty; the metadata scan must still find the session.
        write_index(&root, &serde_json::json!({}));
        std::fs::write(
            session_dir(&root, "session_20260924_100000_eeee").join(METADATA_FILENAME),
            r#"{"session_id":"ses_scan","title":"Recovered"}"#,
        )
        .unwrap();

        let found = find_session_in(&root, "ses_scan").unwrap();
        assert_eq!(found.id, "ses_scan");
        assert_eq!(found.title.as_deref(), Some("Recovered"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn read_meta_usage_maps_stats() {
        let root = temp_dir("usage");
        let key = "session_20260924_100000_ffff";
        write_index(
            &root,
            &serde_json::json!({
                key: { "session_id": "ses_usage", "cwd": "/p", "start_time": "2026-09-24T10:00:00+00:00" }
            }),
        );
        std::fs::write(
            session_dir(&root, key).join(METADATA_FILENAME),
            r#"{"session_id":"ses_usage","title":null,"stats":{"session_prompt_tokens":11,"session_completion_tokens":22,"session_cached_tokens":33,"session_cost":0.5}}"#,
        )
        .unwrap();

        let usage = read_meta_usage(&root, "ses_usage").unwrap();
        assert_eq!(usage.input_tokens, 11);
        assert_eq!(usage.output_tokens, 22);
        assert_eq!(usage.cache_read_tokens, 33);
        assert_eq!(usage.cache_write_tokens, 0);
        assert_eq!(usage.cost_usd, Some(0.5));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn read_meta_usage_is_none_without_stats() {
        let root = temp_dir("nostats");
        let key = "session_20260924_100000_9999";
        write_index(
            &root,
            &serde_json::json!({
                key: { "session_id": "ses_nostats", "cwd": "/p", "start_time": "2026-09-24T10:00:00+00:00" }
            }),
        );
        std::fs::write(
            session_dir(&root, key).join(METADATA_FILENAME),
            r#"{"session_id":"ses_nostats","title":null}"#,
        )
        .unwrap();
        assert!(read_meta_usage(&root, "ses_nostats").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_or_malformed_index_returns_none() {
        let root = temp_dir("malformed");
        let cwd = Path::new("/home/me/proj");
        let since = Utc::now();
        assert!(find_recent_session(&root, cwd, since).is_none());
        std::fs::write(index_path(&root), b"{ not json").unwrap();
        assert!(find_recent_session(&root, cwd, since).is_none());
        assert!(find_session_in(&root, "ses_missing").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
