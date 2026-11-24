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
//!
//! It is kept conformant with the official ACP TCK (agentclientprotocol/acp-tck,
//! run in CI by `.github/workflows/acp-tck.yml`): requests before `initialize`
//! get `-32600`, invalid params `-32602`, unknown sessions `-32002` with
//! `data.sessionId`, capability-gated methods that were not advertised `-32601`,
//! every `session/new` mints a distinct `sessionId`, `type: "boolean"` config
//! options are only included when the client advertised
//! `clientCapabilities.session.configOptions.boolean`, and stdio MCP servers
//! from `session/new` are connected to. Tests that need the legacy
//! accept-any-session behavior (loading provider session ids that were never
//! created by this process) set `FAKE_ACP_LENIENT_SESSIONS=1`. Set
//! `FAKE_ACP_TRANSCRIPT=<path>` to record every incoming client message.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};

const SESSION_ID: &str = "ses_fake_acp";
const DEFAULT_MODEL: &str = "fake/model-a";
const DEFAULT_THOUGHT: &str = "medium";

/// The config options this fake agent advertises. The `fast` boolean option is
/// only included when the client advertised
/// `clientCapabilities.session.configOptions.boolean` (ACP-CONFIG-003: an agent
/// MUST NOT include a `type: "boolean"` option otherwise).
fn config_options(current_model: &str, current_thought: &str, include_boolean: bool) -> Value {
    let mut options = json!([
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
        }
    ]);
    if include_boolean {
        if let Some(options) = options.as_array_mut() {
            options.push(json!({
                "id": "fast",
                "name": "Fast mode",
                "category": "speed",
                "type": "boolean",
                "currentValue": false
            }));
        }
    }
    options
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
    record_pids();
    inject_transport_fault(&mut stdout);

    let mut authenticated = std::env::var_os("FAKE_ACP_AUTH_REQUIRED").is_none();
    let mut initialized = false;
    let mut boolean_config_advertised = false;
    let mut session_seq = 0u32;
    let mut known_sessions: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut closed_sessions: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut lines = stdin.lock().lines();
    let mut prompt_turn = 0u32;
    while let Some(Ok(line)) = lines.next() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        record_transcript(line);
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            continue;
        };
        let id = message.get("id").cloned();

        // ACP-ERROR-005: any request before `initialize` gets -32600.
        if !initialized && method != "initialize" {
            if let Some(id) = id {
                error(
                    &mut stdout,
                    id,
                    -32600,
                    "initialize must be the first request",
                );
            }
            continue;
        }

        match method {
            "initialize" => {
                if std::env::var_os("FAKE_ACP_INIT_ERROR").is_some() {
                    if let Some(id) = id {
                        error(&mut stdout, id, -32000, "cannot initialize fake agent");
                    }
                    continue;
                }
                if let Some(marker) = std::env::var_os("FAKE_ACP_INIT_RECORD") {
                    let capabilities = message
                        .get("params")
                        .and_then(|params| params.get("clientCapabilities"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    let _ = std::fs::write(
                        marker,
                        serde_json::to_vec_pretty(&capabilities).unwrap_or_default(),
                    );
                }
                let mut capabilities = json!({
                    "loadSession": load_supported()
                });
                let mut mcp_capabilities = serde_json::Map::new();
                if std::env::var("FAKE_ACP_MCP_HTTP").as_deref() == Ok("1") {
                    mcp_capabilities.insert("http".to_string(), json!(true));
                }
                if std::env::var("FAKE_ACP_MCP_SSE").as_deref() == Ok("1") {
                    mcp_capabilities.insert("sse".to_string(), json!(true));
                }
                if !mcp_capabilities.is_empty() {
                    capabilities["mcpCapabilities"] = Value::Object(mcp_capabilities);
                }
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
                    "agentCapabilities": capabilities,
                    "agentInfo": { "name": "fake-acp-agent", "version": "0.1.0" }
                });
                if let Ok(methods) = std::env::var("FAKE_ACP_AUTH_METHODS") {
                    response["authMethods"] =
                        serde_json::from_str(&methods).unwrap_or_else(|_| json!([]));
                }
                // ACP-CONFIG-003 gates `type: "boolean"` config options on the
                // client advertising `clientCapabilities.session.configOptions.boolean`.
                boolean_config_advertised = message
                    .pointer("/params/clientCapabilities/session/configOptions/boolean")
                    .is_some();
                initialized = true;
                respond(&mut stdout, id, response);
            }
            "session/new" => {
                record_mcp("session/new", &message);
                // ACP-ERROR-002: `cwd` is a required param of `session/new`.
                if message
                    .pointer("/params/cwd")
                    .and_then(Value::as_str)
                    .is_none()
                {
                    if let Some(id) = id {
                        error(&mut stdout, id, -32602, "missing required param: cwd");
                    }
                    continue;
                }
                if !authenticated {
                    if let Some(id) = id {
                        error(&mut stdout, id, -32000, "Authentication required");
                    }
                    continue;
                }
                // ACP-SESSION-002: every `session/new` on a connection mints a
                // distinct session id (the first keeps the classic `<pid>` form).
                session_seq += 1;
                session_id = if session_seq == 1 {
                    format!("{SESSION_ID}-{}", std::process::id())
                } else {
                    format!("{SESSION_ID}-{}-{}", std::process::id(), session_seq)
                };
                known_sessions.insert(session_id.clone());
                // ACP-SESSION-004: connect to every stdio MCP server handed to us.
                connect_stdio_mcp_servers(&message);
                respond(
                    &mut stdout,
                    id,
                    json!({
                        "sessionId": session_id.clone(),
                        "configOptions": config_options(&selected_model, &selected_thought, boolean_config_advertised),
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
                record_mcp("session/load", &message);
                // ACP-ERROR-006: `session/load` requires the advertised `loadSession` capability.
                if !load_supported() {
                    if let Some(id) = id {
                        error(&mut stdout, id, -32601, "method not found");
                    }
                    continue;
                }
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
                // ACP-ERROR-004: loading a session that does not exist is
                // -32002 with data.sessionId, not a silent create-and-succeed.
                if !lenient_sessions() && !known_sessions.contains(&loaded) {
                    if let Some(id) = id {
                        error_data(
                            &mut stdout,
                            id,
                            -32002,
                            "session not found",
                            json!({ "sessionId": loaded }),
                        );
                    }
                    continue;
                }
                if let Some(marker) = std::env::var_os("FAKE_ACP_LOAD_MARKER") {
                    let cwd = params
                        .and_then(|params| params.get("cwd"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let _ = std::fs::write(marker, format!("{loaded}\n{cwd}\n"));
                }
                session_id = loaded;
                known_sessions.insert(session_id.clone());
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
                            "configOptions": config_options(&selected_model, &selected_thought, boolean_config_advertised),
                            "modes": session_modes(&current_mode)
                        }),
                    );
                }
            }
            "session/list" => {
                // ACP-ERROR-006: `session/list` requires the advertised `list` capability.
                if !env_flag("FAKE_ACP_LIST_SUPPORTED") {
                    if let Some(id) = id {
                        error(&mut stdout, id, -32601, "method not found");
                    }
                    continue;
                }
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
                record_mcp("session/resume", &message);
                // ACP-ERROR-006: `session/resume` requires the advertised `resume` capability.
                if !env_flag("FAKE_ACP_RESUME_SUPPORTED") {
                    if let Some(id) = id {
                        error(&mut stdout, id, -32601, "method not found");
                    }
                    continue;
                }
                let resumed = message
                    .get("params")
                    .and_then(|params| params.get("sessionId"))
                    .and_then(Value::as_str)
                    .unwrap_or(&session_id)
                    .to_string();
                // ACP-ERROR-004: resuming an unknown session is -32002, not a silent adopt.
                if !lenient_sessions() && !known_sessions.contains(&resumed) {
                    if let Some(id) = id {
                        error_data(
                            &mut stdout,
                            id,
                            -32002,
                            "session not found",
                            json!({ "sessionId": resumed }),
                        );
                    }
                    continue;
                }
                session_id = resumed;
                known_sessions.insert(session_id.clone());
                if std::env::var("FAKE_ACP_RESUME_RESULT").as_deref() == Ok("error") {
                    if let Some(id) = id {
                        error(&mut stdout, id, -32000, "cannot resume session");
                    }
                } else {
                    respond(
                        &mut stdout,
                        id,
                        json!({ "configOptions": config_options(&selected_model, &selected_thought, boolean_config_advertised) }),
                    );
                }
            }
            "session/delete" => {
                // ACP-ERROR-006: `session/delete` requires the advertised `delete` capability.
                if !env_flag("FAKE_ACP_DELETE_SUPPORTED") {
                    if let Some(id) = id {
                        error(&mut stdout, id, -32601, "method not found");
                    }
                    continue;
                }
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
                // ACP-ERROR-004: unknown session -> -32002 with data.sessionId.
                if !lenient_sessions()
                    && !session_known(&message, &known_sessions, &closed_sessions)
                {
                    if let Some(id) = id {
                        error_data(
                            &mut stdout,
                            id,
                            -32002,
                            "session not found",
                            json!({ "sessionId": requested_session(&message) }),
                        );
                    }
                    continue;
                }
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
                    // Only settable when the boolean config gate was advertised by
                    // the client (it is not part of the advertised options otherwise).
                    "fast" if boolean_config_advertised => {
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
                    let mut options = config_options(
                        &selected_model,
                        &selected_thought,
                        boolean_config_advertised,
                    );
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
                // ACP-ERROR-004: unknown session -> -32002 with data.sessionId.
                if !lenient_sessions()
                    && !session_known(&message, &known_sessions, &closed_sessions)
                {
                    if let Some(id) = id {
                        error_data(
                            &mut stdout,
                            id,
                            -32002,
                            "session not found",
                            json!({ "sessionId": requested_session(&message) }),
                        );
                    }
                    continue;
                }
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
                // ACP-ERROR-003: prompting a session that was never created, or
                // that this connection already closed, is -32002 with data.sessionId.
                if !lenient_sessions()
                    && !session_known(&message, &known_sessions, &closed_sessions)
                {
                    if let Some(id) = id {
                        error_data(
                            &mut stdout,
                            id,
                            -32002,
                            "session not found",
                            json!({ "sessionId": requested_session(&message) }),
                        );
                    }
                    continue;
                }
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
                // ACP-ERROR-006: `session/close` requires the advertised `close` capability.
                if !close_advertised() {
                    if let Some(id) = id {
                        error(&mut stdout, id, -32601, "method not found");
                    }
                    continue;
                }
                // Record that the optional request arrived, even when this fake
                // is configured to ignore it.
                if let Some(marker) = std::env::var_os("FAKE_ACP_CLOSE_MARKER") {
                    let _ = std::fs::write(marker, b"session/close\n");
                }
                closed_sessions.insert(requested_session(&message));
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

/// Append the request's `mcpServers` params to `FAKE_ACP_MCP_MARKER`, one JSON
/// object per request, so tests can assert exactly what Favetto forwarded.
fn record_mcp(method: &str, message: &Value) {
    let Some(marker) = std::env::var_os("FAKE_ACP_MCP_MARKER") else {
        return;
    };
    let servers = message
        .get("params")
        .and_then(|params| params.get("mcpServers"))
        .cloned()
        .unwrap_or_else(|| json!([]));
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(marker)
    {
        let _ = writeln!(file, "{}", json!({ "method": method, "servers": servers }));
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

    if let Ok(schema) = std::env::var("FAKE_ACP_ELICITATION_SCHEMA") {
        run_elicitation(stdout, lines, session_id, prompt_turn, &schema);
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
            record_transcript(&line);
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
            respond(stdout, id, prompt_response("cancelled"));
        } else {
            emit_echo(stdout, session_id, message, selected_model);
            emit_final_usage(stdout, session_id);
            respond(stdout, id, prompt_response("end_turn"));
        }
        return;
    }

    emit_echo(stdout, session_id, message, selected_model);
    emit_final_usage(stdout, session_id);
    respond(stdout, id, prompt_response("end_turn"));
}

/// Send one `elicitation/create` and record the client's ACP response.
///
/// The recorded file is a JSON object with a `responses` array, so a test can
/// prove that a duplicate reply never reached the agent. A JSON-RPC error is
/// recorded with its code and message instead of being mistaken for a decline.
fn run_elicitation<B: BufRead>(
    stdout: &mut std::io::Stdout,
    lines: &mut std::io::Lines<B>,
    session_id: &str,
    prompt_turn: u32,
    schema: &str,
) {
    let request_id = format!("elic-{}-{prompt_turn}", std::process::id());
    let mode = std::env::var("FAKE_ACP_ELICITATION_MODE").unwrap_or_else(|_| "form".to_string());
    let message = std::env::var("FAKE_ACP_ELICITATION_MESSAGE")
        .unwrap_or_else(|_| "Please fill in the form".to_string());
    let mut params = json!({
        "sessionId": session_id,
        "message": message,
        "mode": mode,
    });
    if mode == "url" {
        params["elicitationId"] = json!(format!("url-elic-{prompt_turn}"));
        params["url"] = json!("https://example.com/consent");
    } else {
        match serde_json::from_str::<Value>(schema) {
            Ok(schema) => params["requestedSchema"] = schema,
            Err(error) => {
                params["requestedSchema"] = json!({ "type": "object", "properties": {} });
                record_elicitation(json!({ "fixture_error": error.to_string() }));
            }
        }
    }
    write_json(
        stdout,
        json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "elicitation/create",
            "params": params,
        }),
    );

    loop {
        let Some(Ok(line)) = lines.next() else {
            return;
        };
        record_transcript(&line);
        let Ok(incoming) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if incoming.get("method").and_then(Value::as_str) == Some("session/cancel") {
            record_cancel();
            continue;
        }
        if incoming.get("id") != Some(&Value::String(request_id.clone())) {
            continue;
        }
        if let Some(error) = incoming.get("error") {
            record_elicitation(json!({
                "error": {
                    "code": error.get("code").cloned().unwrap_or(Value::Null),
                    "message": error.get("message").cloned().unwrap_or(Value::Null),
                    "data": error.get("data").cloned().unwrap_or(Value::Null),
                }
            }));
        } else {
            let action = incoming
                .pointer("/result/action")
                .cloned()
                .unwrap_or(Value::Null);
            let content = incoming
                .pointer("/result/content")
                .cloned()
                .unwrap_or(Value::Null);
            record_elicitation(json!({ "action": action, "content": content }));
        }
        break;
    }
}

/// Append one elicitation response to `FAKE_ACP_ELICITATION_RECORD`.
fn record_elicitation(value: Value) {
    let Some(marker) = std::env::var_os("FAKE_ACP_ELICITATION_RECORD") else {
        return;
    };
    let mut responses = std::fs::read_to_string(&marker)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.get("responses").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    responses.push(value);
    let record = json!({ "responses": responses, "count": responses.len() });
    let _ = std::fs::write(
        marker,
        serde_json::to_vec_pretty(&record).unwrap_or_default(),
    );
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

/// Record the agent's and its supervisor's pids for the reaping regression.
///
/// Tests wait for both to disappear after a failed session, proving the daemon
/// killed and reaped both the direct child and the process group.
fn record_pids() {
    let Some(path) = std::env::var_os("FAKE_ACP_PID_FILE") else {
        return;
    };
    #[cfg(unix)]
    let parent = unsafe { libc::getppid() };
    #[cfg(not(unix))]
    let parent = 0;
    let _ = std::fs::write(path, format!("{} {parent}\n", std::process::id()));
}

/// Emit the malformed, oversized, truncated, or hung I/O selected by the test
/// environment before normal protocol handling begins.
fn inject_transport_fault(stdout: &mut std::io::Stdout) {
    if std::env::var_os("FAKE_ACP_TRUNCATE").is_some() {
        // A partial JSON-RPC frame without its terminating newline, then EOF.
        let _ = stdout.write_all(br#"{"jsonrpc":"2.0","id":1,"res"#);
        let _ = stdout.flush();
        std::process::exit(0);
    }
    if std::env::var_os("FAKE_ACP_INVALID_UTF8").is_some() {
        let _ = stdout.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"\xff\xfe\"}\n");
        let _ = stdout.flush();
    }
    if std::env::var_os("FAKE_ACP_OVERSIZED").is_some() {
        // One byte over the daemon's documented 8 MiB message bound.
        let mut line = vec![b'x'; 8 * 1024 * 1024 + 1];
        line.push(b'\n');
        let _ = stdout.write_all(&line);
        let _ = stdout.flush();
    }
    if std::env::var_os("FAKE_ACP_INIT_HANG").is_some() {
        // Never read stdin and never answer `initialize`.
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }
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

/// A prompt response, optionally carrying the token usage configured through
/// `FAKE_ACP_PROMPT_USAGE` (raw ACP `usage` object).
fn prompt_response(stop_reason: &str) -> Value {
    let mut response = json!({ "stopReason": stop_reason });
    if let Ok(raw) = std::env::var("FAKE_ACP_PROMPT_USAGE") {
        if let Ok(usage) = serde_json::from_str::<Value>(&raw) {
            response["usage"] = usage;
        }
    }
    response
}

/// Emit `FAKE_ACP_USAGE_UPDATE` immediately before the prompt response, so a
/// test can prove a final context snapshot is not lost to the exit race.
fn emit_final_usage(stdout: &mut std::io::Stdout, session_id: &str) {
    let Ok(raw) = std::env::var("FAKE_ACP_USAGE_UPDATE") else {
        return;
    };
    let Ok(update) = serde_json::from_str::<Value>(&raw) else {
        return;
    };
    let mut payload = json!({
        "sessionId": session_id,
        "update": { "sessionUpdate": "usage_update" }
    });
    if let (Some(target), Some(fields)) = (
        payload.get_mut("update").and_then(Value::as_object_mut),
        update.as_object(),
    ) {
        for (key, value) in fields {
            target.insert(key.clone(), value.clone());
        }
    }
    notify(stdout, payload);
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

/// An error response carrying a `data` object (used for -32002 session errors).
fn error_data(stdout: &mut std::io::Stdout, id: Value, code: i64, message: &str, data: Value) {
    write_json(
        stdout,
        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message, "data": data } }),
    );
}

/// Append one raw incoming line to `$FAKE_ACP_TRANSCRIPT`, when configured.
fn record_transcript(line: &str) {
    let Some(path) = std::env::var_os("FAKE_ACP_TRANSCRIPT") else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

/// The `sessionId` a request names, or the empty string.
fn requested_session(message: &Value) -> String {
    message
        .pointer("/params/sessionId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Whether the request names a session this process created and has not closed.
fn session_known(
    message: &Value,
    known: &std::collections::HashSet<String>,
    closed: &std::collections::HashSet<String>,
) -> bool {
    let id = requested_session(message);
    !id.is_empty() && known.contains(&id) && !closed.contains(&id)
}

/// Legacy accept-any-session behavior for tests that load provider session ids
/// which were never created by this process (cross-daemon-restart flows).
fn lenient_sessions() -> bool {
    std::env::var_os("FAKE_ACP_LENIENT_SESSIONS").is_some()
}

/// Best-effort connect to each stdio MCP server in a session setup request
/// (ACP-SESSION-004): spawn it with a clean environment plus the entry's own
/// `env`, send one MCP `initialize`, wait briefly for the reply, then stop it.
/// Never fails the session: spawn/read errors are ignored.
fn connect_stdio_mcp_servers(message: &Value) {
    let Some(servers) = message
        .pointer("/params/mcpServers")
        .and_then(Value::as_array)
    else {
        return;
    };
    for server in servers {
        let is_stdio = server.get("type").and_then(Value::as_str) == Some("stdio")
            || (server.get("type").is_none() && server.get("command").is_some());
        if !is_stdio {
            continue;
        }
        let Some(command) = server.get("command").and_then(Value::as_str) else {
            continue;
        };
        let args: Vec<String> = server
            .get("args")
            .and_then(Value::as_array)
            .map(|args| {
                args.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let mut child_cmd = std::process::Command::new(command);
        child_cmd
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // A clean environment: never leak the agent's own env (e.g. TCK canaries)
        // into the MCP server; only pass through what the entry names plus the
        // basics needed to locate and run the command.
        child_cmd.env_clear();
        for key in ["PATH", "HOME", "SYSTEMROOT", "TMPDIR", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(key) {
                child_cmd.env(key, value);
            }
        }
        if let Some(env) = server.get("env").and_then(Value::as_array) {
            for entry in env {
                if let (Some(name), Some(value)) = (
                    entry.get("name").and_then(Value::as_str),
                    entry.get("value").and_then(Value::as_str),
                ) {
                    child_cmd.env(name, value);
                }
            }
        }
        let Ok(mut child) = child_cmd.spawn() else {
            continue;
        };
        if let Some(mut child_stdin) = child.stdin.take() {
            let init = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "fake-acp-agent", "version": "0.1.0" }
                }
            });
            let _ = writeln!(child_stdin, "{init}");
            let _ = child_stdin.flush();
        }
        // Wait briefly for any reply so the server has processed `initialize`.
        if let Some(child_stdout) = child.stdout.take() {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut reader = std::io::BufReader::new(child_stdout);
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                let _ = tx.send(());
            });
            let _ = rx.recv_timeout(Duration::from_secs(2));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
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
