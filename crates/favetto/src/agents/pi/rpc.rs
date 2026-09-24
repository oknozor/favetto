//! pi's `--mode rpc` transport: a [`StateSource`] over the PTY.
//!
//! RPC mode is a long-lived JSONL protocol. favetto writes commands
//! (`get_state`, `prompt`, `get_messages`, `abort`) to the agent's stdin and
//! reads `response`/event records from stdout through [`PiJsonlParser`]. When pi
//! raises an `extension_ui_request` dialog, [`PiRpcResponder`] answers with a
//! correlating `extension_ui_response` on stdin and synthesizes the
//! [`AgentStateEvent::InputResolved`] the stream does not echo.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::json;
use tokio::sync::mpsc;

use favetto_core::model::{AgentStateEvent, InputReply};

use crate::agents::state::{InputResponder, StateContext, StateSource, StateStart};

use super::json::{PiJsonlParser, SharedWriter};

/// Writes JSON-line commands to pi's stdin, correlating each with a unique id.
pub(crate) struct PiRpcClient {
    stdin: SharedWriter,
    next_id: AtomicU64,
}

impl PiRpcClient {
    pub(crate) fn new(stdin: SharedWriter) -> Self {
        Self {
            stdin,
            next_id: AtomicU64::new(0),
        }
    }

    fn next_id(&self) -> String {
        format!("favetto-{}", self.next_id.fetch_add(1, Ordering::SeqCst))
    }

    /// Serialize `command` as one compact JSON line and write it, returning the
    /// command id.
    pub(crate) fn send(&self, command: serde_json::Value) -> anyhow::Result<String> {
        let id = command
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let mut line = serde_json::to_string(&command)?;
        line.push('\n');
        let mut writer = self.stdin.lock();
        writer.write_all(line.as_bytes())?;
        writer.flush()?;
        Ok(id)
    }

    pub(crate) fn get_state(&self) -> anyhow::Result<String> {
        self.send(json!({ "id": self.next_id(), "type": "get_state" }))
    }

    pub(crate) fn prompt(&self, message: &str) -> anyhow::Result<String> {
        self.send(json!({ "id": self.next_id(), "type": "prompt", "message": message }))
    }

    /// Exposed for the richer interactive transport (#160); covered by a unit
    /// test.
    #[allow(dead_code)]
    pub(crate) fn get_messages(&self) -> anyhow::Result<String> {
        self.send(json!({ "id": self.next_id(), "type": "get_messages" }))
    }

    pub(crate) fn abort(&self) -> anyhow::Result<String> {
        self.send(json!({ "id": self.next_id(), "type": "abort" }))
    }
}

/// The `pi-rpc` state source: a [`StdoutParser`](crate::agents::state::StdoutParser)
/// fed by the manager plus a [`PiRpcResponder`] for dialogs.
pub(crate) struct PiRpcSource;

impl PiRpcSource {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl StateSource for PiRpcSource {
    fn label(&self) -> &'static str {
        "pi-rpc"
    }

    fn start(&self, ctx: StateContext) -> anyhow::Result<StateStart> {
        let (tx, events) = mpsc::unbounded_channel();
        let client = PiRpcClient::new(ctx.stdin.clone());
        // Ask for the session identity and start the task turn before any output
        // can be missed; the PTY buffers stdin until pi is ready.
        let _ = client.get_state();
        if let Some(prompt) = &ctx.prompt {
            let _ = client.prompt(prompt);
        }

        let responder = Arc::new(PiRpcResponder::new(ctx.stdin.clone(), tx.clone()));
        // The parser closes pi's stdin on `agent_settled` so a headless RPC
        // process exits and the task lifecycle completes.
        let parser = PiJsonlParser::with_channel(tx, Some(ctx.stdin.clone()));

        Ok(StateStart {
            events,
            stdout: Some(Box::new(parser)),
            responder,
            stop: Some(Box::new(move || {
                let _ = client.abort();
            })),
        })
    }
}

/// Answers pi's `extension_ui_request` dialogs on stdin.
pub(crate) struct PiRpcResponder {
    stdin: SharedWriter,
    tx: mpsc::UnboundedSender<AgentStateEvent>,
}

/// Build the `extension_ui_response` record for one normalized reply.
fn response_value(id: &str, reply: &InputReply) -> serde_json::Value {
    let mut value = json!({ "type": "extension_ui_response", "id": id });
    match reply {
        InputReply::Value { value: answer } => value["value"] = json!(answer),
        InputReply::Confirmed { confirmed } => value["confirmed"] = json!(confirmed),
        InputReply::Cancelled | InputReply::Reject => value["cancelled"] = json!(true),
        // pi selects expect the option string; the TUI sends `Value` with the
        // real option, so this is only a best-effort fallback.
        InputReply::Once | InputReply::Always => value["value"] = json!("Allow"),
    }
    value
}

impl PiRpcResponder {
    pub(crate) fn new(stdin: SharedWriter, tx: mpsc::UnboundedSender<AgentStateEvent>) -> Self {
        Self { stdin, tx }
    }

    /// Write one `extension_ui_response` and synthesize the `InputResolved`
    /// event pi does not echo.
    pub(crate) fn write_reply(&self, request_id: &str, reply: &InputReply) -> anyhow::Result<()> {
        let response = response_value(request_id, reply);
        let mut line = serde_json::to_string(&response)?;
        line.push('\n');
        {
            let mut writer = self.stdin.lock();
            writer.write_all(line.as_bytes())?;
            writer.flush()?;
        }
        // pi does not echo the response on stdout, so close the
        // `InputRequested -> InputResolved` cycle ourselves.
        let _ = self.tx.send(AgentStateEvent::InputResolved {
            id: request_id.to_string(),
        });
        Ok(())
    }
}

impl InputResponder for PiRpcResponder {
    fn reply<'a>(
        &'a self,
        request_id: &'a str,
        reply: InputReply,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move { self.write_reply(request_id, &reply) })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use parking_lot::Mutex;

    use super::*;

    fn writer() -> (SharedWriter, Arc<Mutex<Vec<u8>>>) {
        let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(SharedBuffer(buffer.clone()))));
        (writer, buffer)
    }

    /// A `Write` that appends to a shared `Vec<u8>` so a test can inspect it.
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn context(prompt: Option<&str>, stdin: SharedWriter) -> StateContext {
        StateContext {
            favetto_session: "fav-ses".to_string(),
            external_session: Some("ses_pi".to_string()),
            prompt: prompt.map(str::to_string),
            headless: true,
            cwd: None,
            program: PathBuf::from("pi"),
            args: vec!["--mode".to_string(), "rpc".to_string()],
            env: BTreeMap::new(),
            stdin,
        }
    }

    fn lines(buffer: &Arc<Mutex<Vec<u8>>>) -> Vec<serde_json::Value> {
        String::from_utf8_lossy(&buffer.lock())
            .lines()
            .map(|line| serde_json::from_str(line).expect("each written line is JSON"))
            .collect()
    }

    #[test]
    fn start_sends_get_state_and_prompt_jsonl() {
        let (stdin, buffer) = writer();
        PiRpcSource::new()
            .start(context(Some("do it"), stdin))
            .unwrap();

        let raw = String::from_utf8_lossy(&buffer.lock()).to_string();
        assert!(!raw.contains('\r'), "commands must not contain CR: {raw:?}");
        assert!(raw.ends_with('\n'), "commands must end with LF: {raw:?}");

        let lines = lines(&buffer);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["type"], "get_state");
        assert_eq!(lines[1]["type"], "prompt");
        assert_eq!(lines[1]["message"], "do it");
        assert!(lines[0]["id"].is_string());
        assert_ne!(lines[0]["id"], lines[1]["id"]);
    }

    #[test]
    fn start_without_prompt_only_sends_get_state() {
        let (stdin, buffer) = writer();
        PiRpcSource::new().start(context(None, stdin)).unwrap();
        let lines = lines(&buffer);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["type"], "get_state");
    }

    #[test]
    fn responder_writes_value_confirm_and_cancel_responses() {
        let (stdin, buffer) = writer();
        let (tx, _rx) = mpsc::unbounded_channel();
        let responder = PiRpcResponder::new(stdin, tx);

        for (id, reply, key, expected) in [
            (
                "ui-1",
                InputReply::Value {
                    value: "Allow".to_string(),
                },
                "value",
                serde_json::json!("Allow"),
            ),
            (
                "ui-2",
                InputReply::Confirmed { confirmed: false },
                "confirmed",
                serde_json::json!(false),
            ),
            (
                "ui-3",
                InputReply::Cancelled,
                "cancelled",
                serde_json::json!(true),
            ),
            (
                "ui-4",
                InputReply::Reject,
                "cancelled",
                serde_json::json!(true),
            ),
            (
                "ui-5",
                InputReply::Once,
                "value",
                serde_json::json!("Allow"),
            ),
        ] {
            responder.write_reply(id, &reply).unwrap();
            let lines = lines(&buffer);
            let line = lines.last().unwrap();
            assert_eq!(line["type"], "extension_ui_response");
            assert_eq!(line["id"], id);
            assert_eq!(line[key], expected);
        }
    }

    #[test]
    fn responder_emits_input_resolved() {
        let (stdin, _buffer) = writer();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let responder = PiRpcResponder::new(stdin, tx);
        responder
            .write_reply(
                "ui-9",
                &InputReply::Value {
                    value: "ok".to_string(),
                },
            )
            .unwrap();
        assert_eq!(
            rx.try_recv().unwrap(),
            AgentStateEvent::InputResolved {
                id: "ui-9".to_string()
            }
        );
    }

    #[test]
    fn client_commands_are_compact_json_lines() {
        let (stdin, buffer) = writer();
        let client = PiRpcClient::new(stdin);
        client.get_state().unwrap();
        client.prompt("hi").unwrap();
        client.get_messages().unwrap();
        client.abort().unwrap();

        let lines = lines(&buffer);
        let types: Vec<_> = lines
            .iter()
            .map(|line| line["type"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(types, vec!["get_state", "prompt", "get_messages", "abort"]);
        // Ids are unique and every command is one LF-terminated line.
        let ids: Vec<_> = lines.iter().map(|line| line["id"].clone()).collect();
        let mut unique = ids.clone();
        unique.sort_by_key(|id| id.to_string());
        unique.dedup();
        assert_eq!(unique.len(), ids.len());
        assert!(String::from_utf8_lossy(&buffer.lock()).ends_with('\n'));
    }

    #[tokio::test]
    async fn scripted_stream_drives_events_and_prompt_reply_round_trips() {
        let (stdin, buffer) = writer();
        let start = PiRpcSource::new()
            .start(context(Some("do it"), stdin))
            .unwrap();

        let mut stdout = start.stdout.unwrap();
        stdout.push(
            br#"{"type":"extension_ui_request","id":"ui-1","method":"select","title":"Allow?","options":["Allow","Block"]}"#,
        );
        stdout.push(b"\n");

        let mut events = start.events;
        let first = events.recv().await.unwrap();
        assert_eq!(
            first,
            AgentStateEvent::InputRequested {
                request: favetto_core::model::InputRequest {
                    id: "ui-1".to_string(),
                    kind: favetto_core::model::AwaitingInputKind::Choice,
                    message: "Allow?".to_string(),
                    options: vec!["Allow".to_string(), "Block".to_string()],
                    allow_always: false,
                },
            }
        );

        start
            .responder
            .reply(
                "ui-1",
                InputReply::Value {
                    value: "Allow".to_string(),
                },
            )
            .await
            .unwrap();

        let written = lines(&buffer);
        let response = written.last().unwrap();
        assert_eq!(response["type"], "extension_ui_response");
        assert_eq!(response["id"], "ui-1");
        assert_eq!(response["value"], "Allow");

        assert_eq!(
            events.recv().await.unwrap(),
            AgentStateEvent::InputResolved {
                id: "ui-1".to_string()
            }
        );
    }
}
