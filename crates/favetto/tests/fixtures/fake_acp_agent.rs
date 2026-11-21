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
                if std::env::var_os("FAKE_ACP_INIT_ERROR").is_some() {
                    if let Some(id) = id {
                        error(&mut stdout, id, -32000, "cannot initialize fake agent");
                    }
                    continue;
                }
                let mut capabilities = json!({
                    "loadSession": load_supported()
                });
                let provider = provider_capabilities();
                if !provider.as_object().is_some_and(|object| object.is_empty()) {
                    capabilities["sessionCapabilities"] = provider;
                }
                respond(
                    &mut stdout,
                    id,
                    json!({
                        "protocolVersion": 1,
                        "agentCapabilities": capabilities
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
            "session/list" => {
                let params = message.get("params");
                let filter = params
                    .and_then(|params| params.get("cwd"))
                    .and_then(Value::as_str)
                    .map(PathBuf::from);
                let offset = params
                    .and_then(|params| params.get("cursor"))
                    .and_then(Value::as_str)
                    .and_then(|cursor| cursor.parse::<usize>().ok())
                    .unwrap_or(0);
                let page_size = list_page_size();
                let mut sessions = Vec::new();
                for index in offset..list_count() {
                    let cwd = provider_cwd(index);
                    if let Some(filter) = &filter {
                        if &cwd != filter {
                            continue;
                        }
                    }
                    if sessions.len() >= page_size {
                        break;
                    }
                    sessions.push(json!({
                        "sessionId": provider_session_id(index),
                        "cwd": cwd,
                        "title": format!("Provider session {}", index + 1),
                        "updatedAt": format!("2026-11-2{}T10:00:00Z", index % 10),
                    }));
                }
                let next = if offset + page_size < list_count() {
                    json!(format!("{}", offset + page_size))
                } else {
                    Value::Null
                };
                let mut result = json!({ "sessions": sessions });
                if !next.is_null() {
                    result["nextCursor"] = next;
                }
                respond(&mut stdout, id, result);
            }
            "session/resume" => {
                let resumed = message
                    .get("params")
                    .and_then(|params| params.get("sessionId"))
                    .and_then(Value::as_str)
                    .unwrap_or(&session_id)
                    .to_string();
                session_id = resumed;
                if std::env::var("FAKE_ACP_RESUME_RESULT").as_deref() == Ok("error") {
                    if let Some(id) = id {
                        error(&mut stdout, id, -32000, "cannot resume session");
                    }
                } else {
                    respond(
                        &mut stdout,
                        id,
                        json!({ "configOptions": config_options(&selected_model) }),
                    );
                }
            }
            "session/delete" => {
                let deleted = message
                    .get("params")
                    .and_then(|params| params.get("sessionId"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if std::env::var("FAKE_ACP_DELETE_RESULT").as_deref() == Ok("error") {
                    if let Some(id) = id {
                        error(&mut stdout, id, -32000, "cannot delete session");
                    }
                } else {
                    if let Some(marker) = std::env::var_os("FAKE_ACP_DELETE_MARKER") {
                        let _ = std::fs::write(marker, deleted);
                    }
                    respond(&mut stdout, id, json!({}));
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
                if let Some(marker) = std::env::var_os("FAKE_ACP_PROMPT_MARKER") {
                    let _ = std::fs::write(marker, prompt_text(&message));
                }
                // Tests can ask the agent to report a display title, proving the
                // daemon persists ACP title updates on the durable session row.
                if let Some(title) = std::env::var_os("FAKE_ACP_TITLE") {
                    notify(
                        &mut stdout,
                        json!({
                            "sessionId": session_id.clone(),
                            "update": {
                                "sessionUpdate": "session_info_update",
                                "title": title.to_string_lossy()
                            }
                        }),
                    );
                }
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
                        "sessionId": session_id.clone(),
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
            "session/close" => {
                // Simulate agents that ignore the optional close request.
                if std::env::var_os("FAKE_ACP_IGNORE_CLOSE").is_none() {
                    respond(&mut stdout, id, json!({}));
                }
            }
            // `session/cancel` is a notification: no response.
            _ => {
                if let Some(id) = id {
                    error(&mut stdout, id, -32601, "method not found");
                }
            }
        }
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

/// The provider-session capabilities this fake advertises. All are off by
/// default so existing tests exercise the unsupported paths.
fn provider_capabilities() -> Value {
    let mut capabilities = serde_json::Map::new();
    if env_flag("FAKE_ACP_LIST_SUPPORTED") {
        capabilities.insert("list".to_string(), json!({}));
    }
    if env_flag("FAKE_ACP_DELETE_SUPPORTED") {
        capabilities.insert("delete".to_string(), json!({}));
    }
    if env_flag("FAKE_ACP_RESUME_SUPPORTED") {
        capabilities.insert("resume".to_string(), json!({}));
    }
    Value::Object(capabilities)
}

fn env_flag(name: &str) -> bool {
    !matches!(std::env::var(name).as_deref(), Ok("0" | "false")) && std::env::var_os(name).is_some()
}

fn list_count() -> usize {
    std::env::var("FAKE_ACP_LIST_COUNT")
        .ok()
        .and_then(|count| count.parse().ok())
        .unwrap_or(3)
}

fn list_page_size() -> usize {
    std::env::var("FAKE_ACP_LIST_PAGE_SIZE")
        .ok()
        .and_then(|size| size.parse().ok())
        .unwrap_or(2)
}

fn provider_session_id(index: usize) -> String {
    format!("provider-ses-{index}")
}

fn provider_cwd(index: usize) -> std::path::PathBuf {
    std::env::var_os("FAKE_ACP_PROVIDER_CWD")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/favetto-provider-{}", index % 2)))
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
