//! The MCP request loop over stdio.
//!
//! [`serve`] reads one JSON-RPC line at a time, dispatches it through a
//! [`Connection`], and writes one response line per request. Notifications (no
//! `id`) are processed without a reply. The loop exits cleanly on EOF.
//!
//! `Connection` also tracks the event cursor across calls, which is what makes
//! `favetto_wait` a *resuming* wait rather than a fresh subscription every time.

use std::time::Duration;

use anyhow::Context;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::broadcast::error::RecvError;

use favetto_core::model::Event;
use favetto_core::rpc::{method, push};
use favetto_tui::client::Client;

use crate::prompts;
use crate::protocol::{self, error_code, JsonRpcRequest, JsonRpcResponse};
use crate::resources;
use crate::rpc::ok_result;
use crate::tools::{self, ToolCall, WaitArgs};

/// One MCP client connection: the daemon client plus the resumable event cursor.
pub struct Connection {
    pub client: Client,
    /// Highest event id observed. Passed as `last_event_id` on each wait.
    pub cursor: i64,
}

impl Connection {
    pub fn new(client: Client) -> Self {
        Self { client, cursor: 0 }
    }

    /// Handle one request. Returns `None` for notifications and other messages
    /// that must not be answered.
    pub async fn handle(&mut self, req: JsonRpcRequest) -> Option<JsonRpcResponse> {
        let id = req.id.clone();
        match req.method.as_str() {
            // Client → server notifications: acknowledged by doing nothing.
            "notifications/initialized"
            | "notifications/cancelled"
            | "notifications/roots/list_changed" => None,

            "initialize" => {
                let version = negotiate_version(&req.params);
                Some(protocol::response_ok(
                    id?,
                    json!({
                        "protocolVersion": version,
                        "capabilities": {
                            "tools": {},
                            "resources": {},
                            "prompts": {},
                        },
                        "serverInfo": {
                            "name": "favetto-mcp",
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                    }),
                ))
            }

            "ping" => Some(protocol::response_ok(id?, json!({}))),

            "tools/list" => {
                let tools: Vec<Value> = tools::list()
                    .iter()
                    .map(|tool| {
                        json!({
                            "name": tool.name,
                            "description": tool.description,
                            "inputSchema": tool.input_schema,
                        })
                    })
                    .collect();
                Some(protocol::response_ok(id?, json!({ "tools": tools })))
            }

            "tools/call" => {
                let result = self.call_tool(&req.params).await;
                Some(protocol::response_ok(id?, result))
            }

            "resources/list" => {
                let resources: Vec<Value> = resources::list()
                    .iter()
                    .map(|r| {
                        json!({
                            "uri": r.uri,
                            "name": r.name,
                            "description": r.description,
                            "mimeType": r.mime_type,
                        })
                    })
                    .collect();
                Some(protocol::response_ok(
                    id?,
                    json!({ "resources": resources }),
                ))
            }

            "resources/templates/list" => {
                let templates: Vec<Value> = resources::templates()
                    .iter()
                    .map(|t| {
                        json!({
                            "uriTemplate": t.uri_template,
                            "name": t.name,
                            "description": t.description,
                            "mimeType": t.mime_type,
                        })
                    })
                    .collect();
                Some(protocol::response_ok(
                    id?,
                    json!({ "resourceTemplates": templates }),
                ))
            }

            "resources/read" => {
                let uri = param_str(&req.params, "uri").unwrap_or_default();
                match resources::read(uri, &self.client).await {
                    Ok(value) => Some(protocol::response_ok(
                        id?,
                        json!({
                            "contents": [{
                                "uri": uri,
                                "mimeType": "application/json",
                                "text": value.to_string(),
                            }]
                        }),
                    )),
                    Err(error) => Some(protocol::response_err(
                        id?,
                        error_code::INVALID_PARAMS,
                        error.to_string(),
                    )),
                }
            }

            "prompts/list" => {
                let prompts: Vec<Value> = prompts::list()
                    .iter()
                    .map(|p| {
                        json!({
                            "name": p.name,
                            "description": p.description,
                            "arguments": p.arguments.iter().map(|a| json!({
                                "name": a.name,
                                "description": a.description,
                                "required": a.required,
                            })).collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                Some(protocol::response_ok(id?, json!({ "prompts": prompts })))
            }

            "prompts/get" => {
                let name = param_str(&req.params, "name").unwrap_or_default();
                let args = req
                    .params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match prompts::get(name, &args) {
                    Ok(value) => Some(protocol::response_ok(id?, value)),
                    Err(error) => Some(protocol::response_err(
                        id?,
                        error_code::INVALID_PARAMS,
                        error.to_string(),
                    )),
                }
            }

            other => id.map(|id| {
                protocol::response_err(
                    id,
                    error_code::METHOD_NOT_FOUND,
                    format!("method not found: {other}"),
                )
            }),
        }
    }

    /// Resolve and execute a `tools/call`.
    ///
    /// Every failure — unknown tool, bad arguments, an RPC error, a transport
    /// error — is returned as a *tool execution error* (`isError: true`), not a
    /// JSON-RPC protocol error, per MCP SEP-1303.
    async fn call_tool(&mut self, params: &Value) -> Value {
        let name = param_str(params, "name").unwrap_or_default();
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));

        match tools::resolve(name, &args) {
            Err(error) => tool_error(error.to_string()),
            Ok(ToolCall::Rpc { method, params }) => {
                match self.client.request(method, params).await {
                    Ok(resp) => match ok_result(&resp, method) {
                        Ok(value) => tool_ok(&value),
                        Err(error) => tool_error(error.to_string()),
                    },
                    Err(error) => tool_error(format!("{method} transport error: {error}")),
                }
            }
            Ok(ToolCall::Wait(args)) => match self.wait(&args).await {
                Ok(value) => tool_ok(&value),
                Err(error) => tool_error(error.to_string()),
            },
        }
    }

    /// Bounded wait on the event cursor. Subscribes to pushes *before* issuing
    /// `events.subscribe` so no replayed event can be missed, then collects
    /// events until one matches `until` or the (clamped) timeout elapses.
    async fn wait(&mut self, args: &WaitArgs) -> anyhow::Result<Value> {
        // A zero timeout means "do not wait": return immediately without
        // subscribing, so the call can never block.
        if args.timeout_ms == 0 {
            return Ok(json!({
                "events": [],
                "cursor": self.cursor,
                "timed_out": true,
            }));
        }

        let mut rx = self.client.subscribe();
        self.client
            .request(
                method::EVENTS_SUBSCRIBE,
                json!({ "last_event_id": self.cursor }),
            )
            .await
            .context("events.subscribe")?;

        let deadline = tokio::time::Instant::now() + Duration::from_millis(args.timeout_ms);
        let mut events: Vec<Event> = Vec::new();
        let mut timed_out = true;

        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline - now;
            match tokio::time::timeout(remaining, rx.recv()).await {
                // Deadline elapsed with no matching event.
                Err(_) => break,
                // The connection's push buffer overflowed; resume from the cursor.
                Ok(Err(RecvError::Lagged(_))) => continue,
                Ok(Err(RecvError::Closed)) => break,
                Ok(Ok(notification)) => {
                    if notification.method != push::EVENT {
                        continue;
                    }
                    let Ok(event) = serde_json::from_value::<Event>(notification.params) else {
                        continue;
                    };
                    self.cursor = self.cursor.max(event.id);
                    let wake = args.until.is_empty()
                        || args.until.iter().any(|kind| kind == event.kind.as_str());
                    events.push(event);
                    if wake {
                        timed_out = false;
                        break;
                    }
                }
            }
        }

        Ok(json!({
            "events": events,
            "cursor": self.cursor,
            "timed_out": timed_out,
        }))
    }
}

/// Negotiate the protocol revision: echo a supported client revision, otherwise
/// reply with [`protocol::PROTOCOL_VERSION`].
fn negotiate_version(params: &Value) -> String {
    let requested = param_str(params, "protocolVersion").unwrap_or_default();
    if protocol::SUPPORTED_VERSIONS.contains(&requested) {
        requested.to_string()
    } else {
        protocol::PROTOCOL_VERSION.to_string()
    }
}

fn param_str<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(Value::as_str)
}

/// A successful `tools/call` result: a text content block holding the JSON.
fn tool_ok(value: &Value) -> Value {
    json!({
        "content": [{ "type": "text", "text": value.to_string() }],
        "isError": false,
    })
}

/// A failed `tools/call` result.
fn tool_error(message: impl Into<String>) -> Value {
    json!({
        "content": [{ "type": "text", "text": message.into() }],
        "isError": true,
    })
}

/// Serve MCP over a reader/writer pair until EOF. Reads one request per line and
/// writes one response per request; malformed lines are logged to stderr and
/// skipped so a bad client message cannot kill the session.
pub async fn serve<R, W>(reader: R, writer: W, client: Client) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    let mut writer = writer;
    let mut connection = Connection::new(client);

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let request: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                eprintln!("favetto-mcp: ignoring malformed request: {error}");
                continue;
            }
        };
        if let Some(response) = connection.handle(request).await {
            let mut bytes = serde_json::to_vec(&response)?;
            bytes.push(b'\n');
            writer.write_all(&bytes).await?;
            writer.flush().await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::JsonRpcRequest;
    use serde_json::json;

    /// A connection over an in-process duplex. Handlers that do not touch the
    /// daemon (`initialize`, `ping`, `tools/list`, …) are exercised here;
    /// `tools/call` against a real daemon is covered by
    /// `crates/favetto/tests/mcp_e2e.rs`.
    fn connection() -> Connection {
        let (side, _peer) = tokio::io::duplex(1024);
        let client = Client::connect_io(side).expect("in-process client");
        Connection::new(client)
    }

    async fn request(conn: &mut Connection, method: &str, params: Value) -> Value {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(1)),
            method: method.to_string(),
            params,
        };
        let response = conn.handle(req).await.expect("a response");
        assert_eq!(response.id, json!(1));
        response
            .result
            .unwrap_or_else(|| panic!("expected a result, got error {:?}", response.error))
    }

    #[tokio::test]
    async fn initialize_negotiates_and_advertises_capabilities() {
        let mut conn = connection();
        let result = request(
            &mut conn,
            "initialize",
            json!({"protocolVersion": "2025-11-25"}),
        )
        .await;
        assert_eq!(result["protocolVersion"], "2025-11-25");
        assert!(result["capabilities"]["tools"].is_object());
        assert!(result["capabilities"]["resources"].is_object());
        assert!(result["capabilities"]["prompts"].is_object());
        assert_eq!(result["serverInfo"]["name"], "favetto-mcp");

        // An unsupported revision falls back to the implemented one.
        let result = request(
            &mut conn,
            "initialize",
            json!({"protocolVersion": "1999-01-01"}),
        )
        .await;
        assert_eq!(result["protocolVersion"], protocol::PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn notifications_get_no_response() {
        let mut conn = connection();
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: "notifications/initialized".to_string(),
            params: json!({}),
        };
        assert!(conn.handle(req).await.is_none());
    }

    #[tokio::test]
    async fn ping_and_tools_list() {
        let mut conn = connection();
        let pong = request(&mut conn, "ping", json!({})).await;
        assert!(pong.is_object());

        let tools = request(&mut conn, "tools/list", json!({})).await;
        let tools = tools["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 13);
        assert!(tools.iter().any(|t| t["name"] == "favetto_inspect"));
    }

    #[tokio::test]
    async fn unknown_tool_is_a_tool_execution_error() {
        let mut conn = connection();
        let result = request(
            &mut conn,
            "tools/call",
            json!({"name": "agents.input", "arguments": {}}),
        )
        .await;
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown tool"));
    }

    #[tokio::test]
    async fn unknown_method_is_a_protocol_error() {
        let mut conn = connection();
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(9)),
            method: "nope".to_string(),
            params: json!({}),
        };
        let response = conn.handle(req).await.expect("a response");
        let error = response.error.expect("error object");
        assert_eq!(error.code, error_code::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn wait_with_zero_timeout_does_not_hang() {
        let mut conn = connection();
        let result = request(
            &mut conn,
            "tools/call",
            json!({"name": "favetto_wait", "arguments": {"timeout_ms": 0}}),
        )
        .await;
        assert_eq!(result["isError"], false);
        let text = result["content"][0]["text"].as_str().unwrap();
        let value: Value = serde_json::from_str(text).unwrap();
        assert_eq!(value["timed_out"], true);
    }

    #[tokio::test]
    async fn resources_and_prompts_are_listed() {
        let mut conn = connection();
        let resources = request(&mut conn, "resources/list", json!({})).await;
        assert_eq!(resources["resources"].as_array().unwrap().len(), 2);
        let templates = request(&mut conn, "resources/templates/list", json!({})).await;
        assert_eq!(templates["resourceTemplates"].as_array().unwrap().len(), 1);
        let prompts = request(&mut conn, "prompts/list", json!({})).await;
        assert_eq!(prompts["prompts"][0]["name"], "supervise_workflow");

        let prompt = request(
            &mut conn,
            "prompts/get",
            json!({"name": "supervise_workflow", "arguments": {}}),
        )
        .await;
        let text = prompt["messages"][0]["content"]["text"].as_str().unwrap();
        assert!(text.contains("EXACTLY ONE decision per cycle"));
    }
}
