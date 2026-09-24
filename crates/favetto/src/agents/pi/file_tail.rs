//! Best-effort live state for the interactive pi TUI.
//!
//! Interactive pi (and `pi --session <id>`) has no machine-readable
//! stdin/stdout channel: it renders a TUI and persists each session entry as
//! JSONL under `$PI_HOME/agent/sessions/--<cwd-slug>--/<ts>_<id>.jsonl`. This
//! [`StateSource`] tails the newest session file for the launch's working
//! directory and maps appended entries into the normalized event vocabulary so
//! activity, usage and cost surface without opening the Agent panel.
//!
//! It is bounded and read-only: every poll reads at most [`MAX_BYTES`], maps at
//! most [`MAX_LINES`], skips pathological lines, and never writes to the file.
//! When no file can be found the source stays quiet and the screen heuristic
//! remains the fallback (the manager also aborts the tail on process exit).

use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures_util::future::BoxFuture;
use tokio::sync::mpsc;

use favetto_core::model::{AgentStateEvent, InputReply, MessageRole};

use crate::agents::state::{InputResponder, StateContext, StateSource, StateStart};

use super::json::parse_usage;
use super::session_file;

/// How often the tail checks its session file.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Slack around the launch baseline: a coarse filesystem clock can stamp the
/// freshly created session file just before our `SystemTime::now()`.
const BASELINE_GRACE: Duration = Duration::from_secs(2);
/// Longest prefix read per poll, so a huge session file is never fully read.
const MAX_BYTES: u64 = 256 * 1024;
/// Most session entries mapped per poll.
const MAX_LINES: usize = 512;
/// Longest line considered; a longer line without a newline is discarded.
const MAX_LINE_BYTES: usize = 64 * 1024;

/// The `pi-session-file` [`StateSource`] for non-RPC launches.
pub(crate) struct PiSessionFileSource {
    /// Explicit session root; `None` uses [`session_file::sessions_root`]. Set by
    /// tests so they never touch `$PI_HOME`.
    root: Option<PathBuf>,
}

impl PiSessionFileSource {
    pub(crate) fn new() -> Self {
        Self { root: None }
    }

    #[cfg(test)]
    pub(crate) fn with_root(root: PathBuf) -> Self {
        Self { root: Some(root) }
    }
}

impl StateSource for PiSessionFileSource {
    fn label(&self) -> &'static str {
        "pi-session-file"
    }

    fn start(&self, ctx: StateContext) -> anyhow::Result<StateStart> {
        let cwd = ctx
            .cwd
            .clone()
            .ok_or_else(|| anyhow::anyhow!("pi session tail needs a working directory"))?;
        let root = self
            .root
            .clone()
            .or_else(session_file::sessions_root)
            .ok_or_else(|| anyhow::anyhow!("pi session root is unavailable"))?;
        let (tx, events) = mpsc::unbounded_channel();
        let known = ctx.external_session.clone();
        // Files created before this instant belong to an older session; only a
        // newer one can be the interactive run we just launched.
        let baseline = SystemTime::now();
        let task = tokio::spawn(run_tail(root, cwd, known, baseline, tx));
        let abort = task.abort_handle();
        Ok(StateStart {
            events,
            stdout: None,
            responder: Arc::new(UnsupportedResponder),
            stop: Some(Box::new(move || abort.abort())),
        })
    }
}

/// Poll the session directory until the run's file appears, then follow it.
async fn run_tail(
    root: PathBuf,
    cwd: PathBuf,
    known: Option<String>,
    baseline: SystemTime,
    tx: mpsc::UnboundedSender<AgentStateEvent>,
) {
    let mut tail: Option<SessionTail> = None;
    let after = baseline.checked_sub(BASELINE_GRACE).unwrap_or(baseline);
    loop {
        if tail.is_none() {
            let found = match &known {
                Some(id) => session_file::find_session_file_in(&root, &cwd, id),
                None => session_file::newest_session_file_in(&root, &cwd, Some(after)),
            };
            if let Some(path) = found {
                // A resumed session already has history: tail only what the new
                // run appends. A freshly discovered file starts at zero.
                let skip_history = known.is_some();
                tail = SessionTail::open(path, skip_history).ok();
            }
        }
        if let Some(tail) = tail.as_mut() {
            for event in tail.poll() {
                if tx.send(event).is_err() {
                    return;
                }
            }
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// An append cursor over one pi session file.
struct SessionTail {
    path: PathBuf,
    offset: u64,
    pending: Vec<u8>,
}

impl SessionTail {
    /// Open `path` at its current end when `skip_history`, else at the start.
    fn open(path: PathBuf, skip_history: bool) -> std::io::Result<Self> {
        let len = std::fs::metadata(&path)?.len();
        Ok(Self {
            path,
            offset: if skip_history { len } else { 0 },
            pending: Vec::new(),
        })
    }

    /// Read newly appended complete lines and map them to events. Bounded by
    /// [`MAX_BYTES`] read and [`MAX_LINES`] mapped per call.
    fn poll(&mut self) -> Vec<AgentStateEvent> {
        let mut events = Vec::new();
        // Pull new bytes (bounded). A read of zero still runs the mapping pass
        // below, so lines buffered by a previous burst are not stranded.
        if let Ok(mut file) = std::fs::File::open(&self.path) {
            if file.seek(SeekFrom::Start(self.offset)).is_ok() {
                let mut chunk = vec![0u8; MAX_BYTES as usize];
                let read = file.read(&mut chunk).unwrap_or(0);
                if read > 0 {
                    self.offset += read as u64;
                    self.pending.extend_from_slice(&chunk[..read]);
                }
            }
        }
        // A single pathological line without a newline must not grow the buffer
        // without bound: drop a full window and keep the remainder.
        while self.pending.len() > MAX_LINE_BYTES && !self.pending.contains(&b'\n') {
            self.pending.drain(..MAX_LINE_BYTES);
        }

        let mut processed = 0usize;
        for _ in 0..MAX_LINES {
            let Some(pos) = self.pending[processed..].iter().position(|b| *b == b'\n') else {
                break;
            };
            let end = processed + pos;
            let line = &self.pending[processed..end];
            if line.len() <= MAX_LINE_BYTES {
                map_line(line, &mut events);
            }
            processed = end + 1;
        }
        if processed > 0 {
            self.pending.drain(..processed);
        }
        events
    }
}

/// Map one persisted session entry into normalized events.
fn map_line(line: &[u8], events: &mut Vec<AgentStateEvent>) {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
        return;
    };
    if !value.is_object() {
        return;
    }
    match value
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
    {
        "session" => {
            if let Some(id) = value.get("id").and_then(|v| v.as_str()) {
                events.push(AgentStateEvent::Session {
                    session_id: Some(id.to_string()),
                    title: None,
                    model: None,
                });
            }
        }
        "session_info" => {
            if let Some(name) = value
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|name| !name.is_empty())
            {
                events.push(AgentStateEvent::Title {
                    title: name.to_string(),
                });
            }
        }
        "model_change" => {
            if let Some(model) = value.get("modelId").and_then(|v| v.as_str()) {
                events.push(AgentStateEvent::Session {
                    session_id: None,
                    title: None,
                    model: Some(model.to_string()),
                });
            }
        }
        "message" => map_message(&value, events),
        "usage" => push_usage(value.get("usage"), events),
        // Summary generation contributes to the session totals.
        "compaction" | "branch_summary" => push_usage(value.get("usage"), events),
        _ => {}
    }
}

/// Map one persisted `AgentMessage` into content and usage events.
fn map_message(value: &serde_json::Value, events: &mut Vec<AgentStateEvent>) {
    let Some(message) = value.get("message") else {
        return;
    };
    match message
        .get("role")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
    {
        "assistant" => {
            if let Some(content) = message.get("content").and_then(|v| v.as_array()) {
                for block in content {
                    match block.get("type").and_then(|v| v.as_str()) {
                        Some("text") => {
                            if let Some(text) = non_empty_text(block.get("text")) {
                                events.push(AgentStateEvent::TextDelta {
                                    role: MessageRole::Assistant,
                                    text,
                                });
                            }
                        }
                        Some("thinking") => {
                            if let Some(text) = non_empty_text(block.get("thinking")) {
                                events.push(AgentStateEvent::ReasoningDelta { text });
                            }
                        }
                        Some("toolCall") => {
                            let id = block
                                .get("id")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string();
                            let name = block
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string();
                            let input = block
                                .get("arguments")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null);
                            events.push(AgentStateEvent::ToolStarted { id, name, input });
                        }
                        _ => {}
                    }
                }
            }
            push_usage(message.get("usage"), events);
        }
        "toolResult" => {
            let id = message
                .get("toolCallId")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let name = message
                .get("toolName")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let ok = message.get("isError").and_then(|v| v.as_bool()) != Some(true);
            let output = message.get("content").cloned();
            events.push(AgentStateEvent::ToolFinished {
                id,
                name,
                ok,
                output,
            });
            // Nested tool model work also contributes to the session totals.
            push_usage(message.get("usage"), events);
        }
        // The user prompt is persisted before the model turn, so it is the
        // earliest best-effort signal that pi is thinking.
        "user" => events.push(AgentStateEvent::TurnStarted),
        // `system`, `custom`, `bashExecution`: no live activity to show.
        _ => {}
    }
}

/// Push a `Usage` event for a usage object, if one is present.
fn push_usage(usage: Option<&serde_json::Value>, events: &mut Vec<AgentStateEvent>) {
    if let Some(usage) = usage.filter(|usage| usage.is_object()) {
        events.push(AgentStateEvent::Usage {
            usage: parse_usage(usage),
        });
    }
}

/// A non-empty string at `value`, trimmed of nothing (display text is preserved).
fn non_empty_text(value: Option<&serde_json::Value>) -> Option<String> {
    value
        .and_then(|v| v.as_str())
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// The session-file transport cannot answer dialogs: pi buffers pending UI
/// requests until the TUI answers them, so they never reach the file.
struct UnsupportedResponder;

impl InputResponder for UnsupportedResponder {
    fn reply<'a>(
        &'a self,
        _request_id: &'a str,
        _reply: InputReply,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async {
            Err(anyhow::anyhow!(
                "pi session-file transport cannot answer dialogs"
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "favetto-pi-tail-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn session_dir(root: &Path, cwd: &Path) -> PathBuf {
        let dir = root.join(format!("--{}--", session_file::cwd_slug(cwd)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const ASSISTANT: &str = r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2024-12-03T14:00:02.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"Hello"},{"type":"toolCall","id":"call_1","name":"bash","arguments":{"command":"ls"}}],"usage":{"input":100,"output":20,"cacheRead":10,"cacheWrite":5,"reasoning":7,"totalTokens":135,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0.02}},"stopReason":"toolUse","timestamp":1733234402000}}"#;

    #[test]
    fn session_tail_maps_entries_and_only_new_lines() {
        let root = temp_dir("map");
        let cwd = Path::new("/home/me/proj");
        let path = session_dir(&root, cwd).join("111_ses.jsonl");
        let entries = [
            r#"{"type":"session","version":3,"id":"ses_1","cwd":"/home/me/proj"}"#,
            r#"{"type":"session_info","id":"a","name":"Fix it"}"#,
            r#"{"type":"model_change","id":"b","provider":"anthropic","modelId":"claude-sonnet-4"}"#,
            ASSISTANT,
            r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"t","message":{"role":"toolResult","toolCallId":"call_1","toolName":"bash","content":[{"type":"text","text":"ok"}],"isError":false,"timestamp":1}}"#,
            r#"{"type":"usage","id":"u1","kind":"cache_warm","usage":{"input":1,"output":0,"cacheRead":50000,"cacheWrite":0,"totalTokens":50001,"cost":{"input":0,"output":0,"cacheRead":0.015,"cacheWrite":0,"total":0.015}}}"#,
            r#"{"type":"compaction","id":"c1","summary":"...","usage":{"input":2,"output":3,"cacheRead":0,"cacheWrite":0,"totalTokens":5,"cost":{"total":0.05}}}"#,
        ];
        std::fs::write(&path, entries.join("\n") + "\n").unwrap();

        let mut tail = SessionTail::open(path.clone(), false).unwrap();
        let events = tail.poll();
        assert_eq!(
            events,
            vec![
                AgentStateEvent::Session {
                    session_id: Some("ses_1".to_string()),
                    title: None,
                    model: None,
                },
                AgentStateEvent::Title {
                    title: "Fix it".to_string(),
                },
                AgentStateEvent::Session {
                    session_id: None,
                    title: None,
                    model: Some("claude-sonnet-4".to_string()),
                },
                AgentStateEvent::ReasoningDelta {
                    text: "hmm".to_string(),
                },
                AgentStateEvent::TextDelta {
                    role: MessageRole::Assistant,
                    text: "Hello".to_string(),
                },
                AgentStateEvent::ToolStarted {
                    id: "call_1".to_string(),
                    name: "bash".to_string(),
                    input: serde_json::json!({ "command": "ls" }),
                },
                AgentStateEvent::Usage {
                    usage: favetto_core::model::AgentUsage {
                        input_tokens: 100,
                        output_tokens: 20,
                        reasoning_tokens: 7,
                        cache_read_tokens: 10,
                        cache_write_tokens: 5,
                        cost_usd: Some(0.02),
                    },
                },
                AgentStateEvent::ToolFinished {
                    id: "call_1".to_string(),
                    name: "bash".to_string(),
                    ok: true,
                    output: Some(serde_json::json!([{ "type": "text", "text": "ok" }])),
                },
                AgentStateEvent::Usage {
                    usage: favetto_core::model::AgentUsage {
                        input_tokens: 1,
                        cache_read_tokens: 50000,
                        cost_usd: Some(0.015),
                        ..Default::default()
                    },
                },
                AgentStateEvent::Usage {
                    usage: favetto_core::model::AgentUsage {
                        input_tokens: 2,
                        output_tokens: 3,
                        cost_usd: Some(0.05),
                        ..Default::default()
                    },
                },
            ]
        );

        // An append maps only the new line.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write;
        file.write_all(
            br#"{"type":"message","id":"m3","parentId":"m2","timestamp":"t","message":{"role":"assistant","content":[{"type":"text","text":"done"}],"usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"total":0.001}},"stopReason":"stop","timestamp":2}}"#,
        )
        .unwrap();
        file.write_all(b"\n").unwrap();
        drop(file);

        assert_eq!(
            tail.poll(),
            vec![
                AgentStateEvent::TextDelta {
                    role: MessageRole::Assistant,
                    text: "done".to_string(),
                },
                AgentStateEvent::Usage {
                    usage: favetto_core::model::AgentUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                        cost_usd: Some(0.001),
                        ..Default::default()
                    },
                },
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn user_message_marks_the_turn_started() {
        let mut events = Vec::new();
        map_line(
            br#"{"type":"message","id":"u","message":{"role":"user","content":"hi"}}"#,
            &mut events,
        );
        assert_eq!(events, vec![AgentStateEvent::TurnStarted]);
    }

    #[test]
    fn session_tail_bounds_lines_per_poll() {
        let root = temp_dir("bound");
        let cwd = Path::new("/home/me/proj");
        let path = session_dir(&root, cwd).join("111_ses.jsonl");
        let entry = r#"{"type":"session_info","id":"n","name":"x"}"#;
        let mut body = String::new();
        for _ in 0..(MAX_LINES + 3) {
            body.push_str(entry);
            body.push('\n');
        }
        std::fs::write(&path, body).unwrap();

        let mut tail = SessionTail::open(path, false).unwrap();
        // One poll maps at most MAX_LINES entries; the remainder is buffered.
        assert_eq!(tail.poll().len(), MAX_LINES);
        assert_eq!(tail.poll().len(), 3);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn session_tail_skips_history_when_resuming() {
        let root = temp_dir("skip");
        let cwd = Path::new("/home/me/proj");
        let path = session_dir(&root, cwd).join("111_ses.jsonl");
        std::fs::write(&path, ASSISTANT).unwrap();

        let mut tail = SessionTail::open(path.clone(), true).unwrap();
        assert!(tail.poll().is_empty(), "existing history is not replayed");

        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(br#"{"type":"session_info","id":"n","name":"Live"}"#)
            .unwrap();
        file.write_all(b"\n").unwrap();
        drop(file);
        assert_eq!(
            tail.poll(),
            vec![AgentStateEvent::Title {
                title: "Live".to_string(),
            }]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn newest_session_file_respects_the_baseline() {
        let root = temp_dir("newest");
        let cwd = Path::new("/home/me/proj");
        let dir = session_dir(&root, cwd);
        std::fs::write(dir.join("100_old.jsonl"), "").unwrap();

        // No file is at or after a baseline in the future.
        let future = SystemTime::now() + Duration::from_secs(3600);
        assert!(session_file::newest_session_file_in(&root, cwd, Some(future)).is_none());
        // A baseline in the past still finds the existing file.
        let past = SystemTime::now() - Duration::from_secs(3600);
        assert_eq!(
            session_file::newest_session_file_in(&root, cwd, Some(past))
                .unwrap()
                .file_name()
                .unwrap(),
            "100_old.jsonl"
        );

        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(dir.join("200_new.jsonl"), "").unwrap();
        assert_eq!(
            session_file::newest_session_file_in(&root, cwd, None)
                .unwrap()
                .file_name()
                .unwrap(),
            "200_new.jsonl"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn source_discovers_and_tails_a_new_session_file() {
        let root = temp_dir("source");
        let cwd = Path::new("/home/me/proj");
        let stdin: crate::agents::pi::json::SharedWriter = Arc::new(parking_lot::Mutex::new(
            Box::new(Vec::<u8>::new()) as Box<dyn std::io::Write + Send>,
        ));

        let ctx = StateContext {
            favetto_session: "fav".to_string(),
            external_session: None,
            prompt: None,
            headless: false,
            cwd: Some(cwd.to_path_buf()),
            program: PathBuf::from("pi"),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            stdin,
        };
        // The source captures its "new file" baseline at start, so the session
        // file must be created afterwards.
        let source = PiSessionFileSource::with_root(root.clone());
        let mut start = source.start(ctx).unwrap();
        assert!(start.stdout.is_none());

        let dir = session_dir(&root, cwd);
        let path = dir.join("111_ses.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"session","version":3,"id":"ses_live","cwd":"/home/me/proj"}"#,
                "\n",
                r#"{"type":"session_info","id":"n","name":"Wired"}"#,
                "\n",
            ),
        )
        .unwrap();

        let first = tokio::time::timeout(Duration::from_secs(3), start.events.recv())
            .await
            .expect("tail emits within the timeout")
            .expect("channel open");
        assert!(matches!(
            first,
            AgentStateEvent::Session { ref session_id, .. } if session_id.as_deref() == Some("ses_live")
        ));
        let second = tokio::time::timeout(Duration::from_secs(3), start.events.recv())
            .await
            .expect("tail emits within the timeout")
            .expect("channel open");
        assert!(matches!(
            second,
            AgentStateEvent::Title { ref title } if title == "Wired"
        ));
        if let Some(stop) = start.stop.take() {
            stop();
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
