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
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
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
                respond(
                    &mut stdout,
                    id,
                    json!({
                        "protocolVersion": 1,
                        "agentCapabilities": {}
                    }),
                );
            }
            "session/new" => {
                respond(
                    &mut stdout,
                    id,
                    json!({
                        "sessionId": SESSION_ID,
                        "configOptions": config_options(&selected_model)
                    }),
                );
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
                // Tests can make the turn block deterministically: create
                // `$FAKE_ACP_GATE_DIR/agent-ready`, then wait for
                // `agent-release` before answering.
                if let Some(dir) = gate_dir() {
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = std::fs::write(dir.join("agent-ready"), b"");
                    let release = dir.join("agent-release");
                    while !release.exists() {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
                let text = prompt_text(&message);
                notify(
                    &mut stdout,
                    json!({
                        "sessionId": SESSION_ID,
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
                respond(&mut stdout, id, json!({ "stopReason": "end_turn" }));
            }
            "session/close" => respond(&mut stdout, id, json!({})),
            // `session/cancel` is a notification: no response.
            _ => {
                if let Some(id) = id {
                    error(&mut stdout, id, -32601, "method not found");
                }
            }
        }
    }
}

/// The optional file gate directory, from `FAKE_ACP_GATE_DIR`.
fn gate_dir() -> Option<PathBuf> {
    std::env::var_os("FAKE_ACP_GATE_DIR").map(PathBuf::from)
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
