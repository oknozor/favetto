//! A minimal, deterministic ACP server used by the end-to-end tests.
//!
//! It implements just enough of the protocol for one prompt turn: `initialize`,
//! `session/new`, `session/prompt` (echoing the prompt back as an assistant
//! message) and `session/close`. Every request is answered synchronously so the
//! tests are not timing-dependent.
//!
//! This is a test fixture, not a real agent. It is compiled as a `[[bin]]` so the
//! daemon can launch it exactly like any other ACP server.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};

const SESSION_ID: &str = "ses_fake_acp";
const DEFAULT_MODEL: &str = "fake/model-a";

/// The model options this fake agent advertises.
fn config_options(current: &str) -> Value {
    json!([{
        "id": "model",
        "name": "Model",
        "category": "model",
        "type": "select",
        "currentValue": current,
        "options": [
            { "value": "fake/model-a", "name": "Fake Model A" },
            { "value": "fake/model-b", "name": "Fake Model B" }
        ]
    }])
}

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut selected_model = DEFAULT_MODEL.to_string();
    // Unique per process, so two sessions from the same run (and the durable
    // per-agent uniqueness constraint) do not collide. A load replaces it with
    // the stored id supplied by the client.
    let mut session_id = format!("{SESSION_ID}-{}", std::process::id());
    let mut lines = stdin.lock().lines();
    let mut prompt_turn = 0u32;
    while let Some(Ok(line)) = lines.next() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            continue;
        };
        let id = message.get("id").cloned();

        match method {
            "initialize" => {
                if std::env::var_os("FAKE_ACP_INIT_ERROR").is_some() {
                    if let Some(id) = id {
                        error(&mut stdout, id, -32000, "cannot initialize fake agent");
                    }
                    continue;
                }
                respond(
                    &mut stdout,
                    id,
                    json!({
                        "protocolVersion": 1,
                        "agentCapabilities": {
                            "loadSession": load_supported()
                        }
                    }),
                );
            }
            "session/new" => {
                respond(
                    &mut stdout,
                    id,
                    json!({
                        "sessionId": session_id.clone(),
                        "configOptions": config_options(&selected_model)
                    }),
                );
            }
            "session/load" => {
                let params = message.get("params");
                let loaded = params
                    .and_then(|params| params.get("sessionId"))
                    .and_then(Value::as_str)
                    .unwrap_or(&session_id)
                    .to_string();
                if let Some(marker) = std::env::var_os("FAKE_ACP_LOAD_MARKER") {
                    let cwd = params
                        .and_then(|params| params.get("cwd"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let _ = std::fs::write(marker, format!("{loaded}\n{cwd}\n"));
                }
                session_id = loaded;
                // Replaying history: an optional user message followed by one
                // assistant chunk per `|`-separated piece, all before the load
                // response exactly as a real agent's `session/load` replay is.
                if let Ok(user) = std::env::var("FAKE_ACP_LOAD_USER") {
                    notify(
                        &mut stdout,
                        json!({
                            "sessionId": session_id.clone(),
                            "update": {
                                "sessionUpdate": "user_message_chunk",
                                "content": { "type": "text", "text": user }
                            }
                        }),
                    );
                }
                if let Ok(history) = std::env::var("FAKE_ACP_LOAD_UPDATES") {
                    for piece in history.split('|') {
                        notify(
                            &mut stdout,
                            json!({
                                "sessionId": session_id.clone(),
                                "update": {
                                    "sessionUpdate": "agent_message_chunk",
                                    "content": { "type": "text", "text": piece }
                                }
                            }),
                        );
                    }
                }
                // Deterministic delay: wait for `load-release` after announcing
                // `load-ready`, so tests can attach while the load is in flight.
                if let Some(dir) = load_gate_dir() {
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = std::fs::write(dir.join("load-ready"), b"");
                    let release = dir.join("load-release");
                    while !release.exists() {
                        std::thread::sleep(Duration::from_millis(25));
                    }
                }
                if std::env::var("FAKE_ACP_LOAD_RESULT").as_deref() == Ok("error") {
                    if let Some(id) = id {
                        error(&mut stdout, id, -32000, "cannot load session");
                    }
                } else {
                    respond(
                        &mut stdout,
                        id,
                        json!({ "configOptions": config_options(&selected_model) }),
                    );
                }
            }
            "session/set_config_option" => {
                let wanted = message
                    .get("params")
                    .and_then(|params| params.get("value"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if wanted == "fake/model-a" || wanted == "fake/model-b" {
                    selected_model = wanted;
                    respond(
                        &mut stdout,
                        id,
                        json!({ "configOptions": config_options(&selected_model) }),
                    );
                } else {
                    if let Some(id) = id {
                        error(&mut stdout, id, -32602, "unknown model");
                    }
                }
            }
            "session/prompt" => {
                prompt_turn += 1;
                run_prompt(
                    &mut stdout,
                    &mut lines,
                    id,
                    &message,
                    &session_id,
                    &selected_model,
                    prompt_turn,
                );
            }
            "session/close" => {
                // Simulate agents that ignore the optional close request.
                if std::env::var_os("FAKE_ACP_IGNORE_CLOSE").is_none() {
                    respond(&mut stdout, id, json!({}));
                }
            }
            // `session/cancel` is a notification: no response. Record it so
            // tests can assert when Favetto sent one (and when it did not).
            "session/cancel" => record_cancel(),
            _ => {
                if let Some(id) = id {
                    error(&mut stdout, id, -32601, "method not found");
                }
            }
        }
    }
}

/// Run one prompt turn, optionally blocking on a `session/request_permission`.
///
/// The fake agent is deliberately line-oriented, so this helper consumes stdin
/// itself while the turn is in flight. That lets the fixture observe a client
/// `session/cancel` and the permission response that ACP requires it to send,
/// even though the prompt request has not returned yet.
fn run_prompt<B: BufRead>(
    stdout: &mut std::io::Stdout,
    lines: &mut std::io::Lines<B>,
    id: Option<Value>,
    message: &Value,
    session_id: &str,
    selected_model: &str,
    prompt_turn: u32,
) {
    if let Some(marker) = std::env::var_os("FAKE_ACP_PROMPT_MARKER") {
        let _ = std::fs::write(marker, prompt_text(message));
    }
    // Optional append-free counter, so a test can prove a turn was submitted
    // exactly once across attach/backfill paths.
    if let Some(marker) = std::env::var_os("FAKE_ACP_PROMPT_COUNT") {
        let count = std::fs::read_to_string(&marker)
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
            .unwrap_or_default();
        let _ = std::fs::write(marker, (count + 1).to_string());
    }
    // Tests can ask the agent to report a display title, proving the
    // daemon persists ACP title updates on the durable session row.
    if let Some(title) = std::env::var_os("FAKE_ACP_TITLE") {
        notify(
            stdout,
            json!({
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "session_info_update",
                    "title": title.to_string_lossy()
                }
            }),
        );
    }
    // Tests can make the turn block deterministically: create
    // `$FAKE_ACP_GATE_DIR/agent-ready`, then wait for `agent-release` before
    // answering.
    if let Some(dir) = gate_dir() {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join("agent-ready"), b"");
        let release = dir.join("agent-release");
        while !release.exists() {
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    if std::env::var_os("FAKE_ACP_PERMISSION").is_some() {
        let permission_id = format!("perm-{}-{prompt_turn}", std::process::id());
        record("permission");
        write_json(
            stdout,
            json!({
                "jsonrpc": "2.0",
                "id": permission_id,
                "method": "session/request_permission",
                "params": {
                    "sessionId": session_id,
                    "toolCall": {
                        "toolCallId": format!("call_{prompt_turn}"),
                        "title": "Write a file",
                        "status": "pending"
                    },
                    "options": [
                        { "optionId": "allow-once", "name": "Allow once", "kind": "allow_once" },
                        { "optionId": "reject-once", "name": "Reject", "kind": "reject_once" }
                    ]
                }
            }),
        );
        let mut cancelled = false;
        loop {
            let Some(Ok(line)) = lines.next() else {
                return;
            };
            let Ok(incoming) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if incoming.get("method").and_then(Value::as_str) == Some("session/cancel") {
                record_cancel();
                cancelled = true;
                continue;
            }
            if incoming.get("id") == Some(&Value::String(permission_id.clone())) {
                let outcome = incoming
                    .pointer("/result/outcome/outcome")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                record(&format!("outcome {outcome}"));
                if outcome == "cancelled" {
                    cancelled = true;
                }
                break;
            }
        }
        if cancelled {
            respond(stdout, id, json!({ "stopReason": "cancelled" }));
        } else {
            emit_echo(stdout, session_id, message, selected_model);
            respond(stdout, id, json!({ "stopReason": "end_turn" }));
        }
        return;
    }

    emit_echo(stdout, session_id, message, selected_model);
    respond(stdout, id, json!({ "stopReason": "end_turn" }));
}

/// Emit the deterministic assistant message used by the tests.
fn emit_echo(
    stdout: &mut std::io::Stdout,
    session_id: &str,
    message: &Value,
    selected_model: &str,
) {
    let text = prompt_text(message);
    notify(
        stdout,
        json!({
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": {
                    "type": "text",
                    "text": format!(
                        "echo-agent: {text}\nmodel: {selected_model}\nECHO_AGENT_DONE"
                    )
                }
            }
        }),
    );
}

/// Record a `session/cancel` notification for cancellation tests.
fn record_cancel() {
    record("cancel");
}

/// Append one line to `$FAKE_ACP_CANCEL_MARKER`, when configured.
fn record(line: &str) {
    let Some(marker) = std::env::var_os("FAKE_ACP_CANCEL_MARKER") else {
        return;
    };
    use std::io::Write as _;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(marker)
    {
        let _ = writeln!(file, "{line}");
    }
}

/// Whether to advertise `session/load`. Enabled by default; tests set
/// `FAKE_ACP_LOAD_SUPPORTED=0` to exercise the unsupported path.
fn load_supported() -> bool {
    !matches!(
        std::env::var("FAKE_ACP_LOAD_SUPPORTED").as_deref(),
        Ok("0" | "false")
    )
}

/// The optional file gate directory, from `FAKE_ACP_GATE_DIR`.
fn gate_dir() -> Option<PathBuf> {
    std::env::var_os("FAKE_ACP_GATE_DIR").map(PathBuf::from)
}

/// The optional `session/load` delay gate, from `FAKE_ACP_LOAD_GATE_DIR`.
fn load_gate_dir() -> Option<PathBuf> {
    std::env::var_os("FAKE_ACP_LOAD_GATE_DIR").map(PathBuf::from)
}

/// Concatenate every text block in the prompt request.
fn prompt_text(message: &Value) -> String {
    message
        .get("params")
        .and_then(|params| params.get("prompt"))
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

fn respond(stdout: &mut std::io::Stdout, id: Option<Value>, result: Value) {
    let Some(id) = id else { return };
    write_json(
        stdout,
        json!({ "jsonrpc": "2.0", "id": id, "result": result }),
    );
}

fn error(stdout: &mut std::io::Stdout, id: Value, code: i64, message: &str) {
    write_json(
        stdout,
        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
    );
}

fn notify(stdout: &mut std::io::Stdout, params: Value) {
    write_json(
        stdout,
        json!({ "jsonrpc": "2.0", "method": "session/update", "params": params }),
    );
}

fn write_json(stdout: &mut std::io::Stdout, value: Value) {
    let _ = writeln!(stdout, "{value}");
    let _ = stdout.flush();
}
