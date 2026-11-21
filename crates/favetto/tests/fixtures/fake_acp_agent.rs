//! A minimal, deterministic ACP server used by the end-to-end tests.
//!
//! It implements just enough of the protocol for one prompt turn: `initialize`,
//! `session/new`, `session/prompt` (echoing the prompt back as an assistant
//! message) and `session/close`. Every request is answered synchronously so the
//! tests are not timing-dependent, except where an environment variable asks it
//! to model an unresponsive close or omit the optional close capability.
//!
//! This is a test fixture, not a real agent. It is compiled as a `[[bin]]` so the
//! daemon can launch it exactly like any other ACP server.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};

const SESSION_ID: &str = "ses_fake_acp";
const DEFAULT_MODEL: &str = "fake/model-a";
const DEFAULT_THOUGHT: &str = "medium";

/// The config options this fake agent advertises.
fn config_options(current_model: &str, current_thought: &str) -> Value {
    json!([
        {
            "id": "model",
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": current_model,
            "options": [
                { "value": "fake/model-a", "name": "Fake Model A" },
                { "value": "fake/model-b", "name": "Fake Model B" }
            ]
        },
        {
            "id": "thought",
            "name": "Thought level",
            "category": "thought_level",
            "type": "select",
            "currentValue": current_thought,
            "options": [
                { "value": "low", "name": "Low" },
                { "value": "medium", "name": "Medium" },
                { "value": "high", "name": "High" }
            ]
        },
        {
            "id": "fast",
            "name": "Fast mode",
            "category": "speed",
            "type": "boolean",
            "currentValue": false
        }
    ])
}

/// The modes this fake agent advertises.
fn session_modes(current: &str) -> Value {
    json!({
        "currentModeId": current,
        "availableModes": [
            { "id": "build", "name": "Build" },
            { "id": "plan", "name": "Plan" }
        ]
    })
}

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut selected_model = DEFAULT_MODEL.to_string();
    let mut selected_thought = DEFAULT_THOUGHT.to_string();
    let mut selected_fast = false;
    let mut current_mode = "build".to_string();
    // Unique per process, so two sessions from the same run (and the durable
    // per-agent uniqueness constraint) do not collide. A load replaces it with
    // the stored id supplied by the client.
    let mut session_id = format!("{SESSION_ID}-{}", std::process::id());
    if let Some(marker) = std::env::var_os("FAKE_ACP_PID_MARKER") {
        let _ = std::fs::write(marker, std::process::id().to_string());
    }
    let mut authenticated = std::env::var_os("FAKE_ACP_AUTH_REQUIRED").is_none();
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
                let mut capabilities = json!({
                    "loadSession": load_supported()
                });
                let mut session_capabilities = serde_json::Map::new();
                if close_advertised() {
                    session_capabilities.insert("close".to_string(), json!({}));
                }
                if let Some(provider) = provider_capabilities().as_object() {
                    for (key, value) in provider {
                        session_capabilities.insert(key.clone(), value.clone());
                    }
                }
                if std::env::var_os("FAKE_ACP_AUTH_LOGOUT").is_some() {
                    capabilities["auth"] = json!({ "logout": {} });
                }
                if !session_capabilities.is_empty() {
                    capabilities["sessionCapabilities"] = Value::Object(session_capabilities);
                }
                let mut response = json!({
                    "protocolVersion": 1,
                    "agentCapabilities": capabilities
                });
                if let Ok(methods) = std::env::var("FAKE_ACP_AUTH_METHODS") {
                    response["authMethods"] =
                        serde_json::from_str(&methods).unwrap_or_else(|_| json!([]));
                }
                respond(&mut stdout, id, response);
            }
            "session/new" => {
                if !authenticated {
                    if let Some(id) = id {
                        error(&mut stdout, id, -32000, "Authentication required");
                    }
                    continue;
                }
                respond(
                    &mut stdout,
                    id,
                    json!({
                        "sessionId": session_id.clone(),
                        "configOptions": config_options(&selected_model, &selected_thought),
                        "modes": session_modes(&current_mode)
                    }),
                );
            }
            "authenticate" => {
                let method_id = message
                    .get("params")
                    .and_then(|params| params.get("methodId"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let expected =
                    std::env::var("FAKE_ACP_AUTH_METHOD").unwrap_or_else(|_| "fake-login".into());
                if std::env::var("FAKE_ACP_AUTH_RESULT").as_deref() == Ok("error") {
                    if let Some(id) = id {
                        error(&mut stdout, id, -32603, "fake sign-in rejected");
                    }
                } else if method_id == expected {
                    authenticated = true;
                    respond(&mut stdout, id, json!({}));
                } else if let Some(id) = id {
                    error(&mut stdout, id, -32602, "unknown authentication method");
                }
            }
            "logout" => {
                if std::env::var_os("FAKE_ACP_AUTH_LOGOUT").is_none() {
                    if let Some(id) = id {
                        error(&mut stdout, id, -32601, "method not found");
                    }
                    continue;
                }
                authenticated = false;
                respond(&mut stdout, id, json!({}));
            }
            "session/load" => {
                if !authenticated {
                    if let Some(id) = id {
                        error(&mut stdout, id, -32000, "Authentication required");
                    }
                    continue;
                }
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
                        // A generic load failure, not auth-required (-32000).
                        error(&mut stdout, id, -32603, "cannot load session");
                    }
                } else {
                    respond(
                        &mut stdout,
                        id,
                        json!({
                            "configOptions": config_options(&selected_model, &selected_thought),
                            "modes": session_modes(&current_mode)
                        }),
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
                        json!({ "configOptions": config_options(&selected_model, &selected_thought) }),
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
                let params = message.get("params");
                let config_id = params
                    .and_then(|params| params.get("configId"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let value = params.and_then(|params| params.get("value"));
                let accepted = match config_id {
                    "model" => {
                        let wanted = value.and_then(Value::as_str).unwrap_or_default();
                        if wanted == "fake/model-a" || wanted == "fake/model-b" {
                            selected_model = wanted.to_string();
                            true
                        } else {
                            false
                        }
                    }
                    "thought" => {
                        let wanted = value.and_then(Value::as_str).unwrap_or_default();
                        if ["low", "medium", "high"].contains(&wanted) {
                            selected_thought = wanted.to_string();
                            true
                        } else {
                            false
                        }
                    }
                    "fast" => {
                        if let Some(wanted) = value.and_then(Value::as_bool) {
                            selected_fast = wanted;
                            true
                        } else {
                            false
                        }
                    }
                    _ => false,
                };
                if accepted {
                    let mut options = config_options(&selected_model, &selected_thought);
                    if let Some(fast) = options.as_array_mut().and_then(|options| {
                        options.iter_mut().find(|option| option["id"] == "fast")
                    }) {
                        fast["currentValue"] = json!(selected_fast);
                    }
                    respond(&mut stdout, id, json!({ "configOptions": options }));
                } else if let Some(id) = id {
                    error(&mut stdout, id, -32602, "unknown config option or value");
                }
            }
            "session/set_mode" => {
                let wanted = message
                    .get("params")
                    .and_then(|params| params.get("modeId"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if wanted == "build" || wanted == "plan" {
                    current_mode = wanted;
                    respond(&mut stdout, id, json!({}));
                } else if let Some(id) = id {
                    error(&mut stdout, id, -32602, "unknown mode");
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
                // Record that the optional request arrived, even when this fake
                // is configured to ignore it.
                if let Some(marker) = std::env::var_os("FAKE_ACP_CLOSE_MARKER") {
                    let _ = std::fs::write(marker, b"session/close\n");
                }
                // Simulate agents that advertise close but never answer it.
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
    // Optional deterministic plan/command notifications, so the daemon and
    // both clients can be tested against real ACP v1 update shapes without a
    // live agent.
    if let Ok(plan) = std::env::var("FAKE_ACP_PLAN") {
        if let Ok(entries) = serde_json::from_str::<Value>(&plan) {
            notify(
                stdout,
                json!({
                    "sessionId": session_id,
                    "update": {
                        "sessionUpdate": "plan",
                        "entries": entries
                    }
                }),
            );
        }
    }
    if let Ok(commands) = std::env::var("FAKE_ACP_COMMANDS") {
        if let Ok(available_commands) = serde_json::from_str::<Value>(&commands) {
            notify(
                stdout,
                json!({
                    "sessionId": session_id,
                    "update": {
                        "sessionUpdate": "available_commands_update",
                        "availableCommands": available_commands
                    }
                }),
            );
        }
    }
    // Optional config/mode update notifications, so tests can prove the daemon
    // maps `config_option_update` and `current_mode_update` without a live agent.
    if let Ok(options) = std::env::var("FAKE_ACP_CONFIG_UPDATE") {
        if let Ok(options) = serde_json::from_str::<Value>(&options) {
            notify(
                stdout,
                json!({
                    "sessionId": session_id,
                    "update": {
                        "sessionUpdate": "config_option_update",
                        "configOptions": options
                    }
                }),
            );
        }
    }
    if let Ok(mode) = std::env::var("FAKE_ACP_CURRENT_MODE") {
        notify(
            stdout,
            json!({
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "current_mode_update",
                    "currentModeId": mode
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

/// Whether to advertise `sessionCapabilities.close`. Enabled by default; tests
/// set `FAKE_ACP_CLOSE_ADVERTISED=0` to model an agent that omits the optional
/// capability entirely.
fn close_advertised() -> bool {
    !matches!(
        std::env::var("FAKE_ACP_CLOSE_ADVERTISED").as_deref(),
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
