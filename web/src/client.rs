//! MessagePack RPC client over `POST {base}/rpc`.
//!
//! The client links `favetto-wire`, so it produces and consumes the exact same
//! [`Frame`] bytes as the Unix-socket and WebSocket transports: a
//! `Frame::Request` encoded with `rmp_serde::to_vec_named` is the request body,
//! and the response body is one `Frame::Response`. Authentication is a bearer
//! header (a browser `fetch` can set one; only the WebSocket upgrade cannot).
//!
//! Every method has a typed wrapper generated from the [`RpcCall`] registry, so
//! a call site names the method once and the compiler checks both the params and
//! the decoded result.

use std::cell::Cell;

use favetto_wire::rpc::*;

use crate::config;

/// Anything that can go wrong between a typed call and its typed result.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClientError {
    /// The request params or the [`Frame`] could not be MessagePack-encoded.
    #[error("msgpack encode: {0}")]
    Encode(String),
    /// The response body could not be MessagePack-decoded into the result type.
    #[error("msgpack decode: {0}")]
    Decode(String),
    /// The request never reached the daemon (offline, DNS, CORS, no window).
    #[error("network: {0}")]
    Network(String),
    /// The daemon answered with a non-2xx HTTP status other than 401.
    #[error("http error {status}: {message}")]
    Http { status: u16, message: String },
    /// Missing or rejected bearer token (HTTP 401).
    #[error("unauthorized: missing or rejected token")]
    Unauthorized,
    /// A structured [`RpcErrorObject`] carried inside a 2xx `Frame::Response`.
    #[error("rpc error {code}: {message}")]
    Rpc { code: i32, message: String },
    /// A response body decoded as a frame that was not a `Response`.
    #[error("unexpected non-response frame")]
    UnexpectedFrame,
    /// A response with neither `result` nor `error` (protocol violation).
    #[error("response carried neither result nor error")]
    EmptyResponse,
    /// The response id did not match the request (out-of-order / cross-talk).
    #[error("response id mismatch: expected {expected}, got {got}")]
    IdMismatch { expected: RequestId, got: RequestId },
}

/// Encode a typed request into a standalone MessagePack `Frame::Request` body.
///
/// Exposed (rather than only used by [`RpcClient::call`]) so the wire encoding
/// has a host-side unit test independent of the browser.
pub fn encode_request<C: RpcCall>(
    id: RequestId,
    params: &C::Params,
) -> Result<Vec<u8>, ClientError> {
    let params = serde_json::to_value(params).map_err(|e| ClientError::Encode(e.to_string()))?;
    let frame = Frame::Request(Request {
        id,
        method: C::METHOD.to_string(),
        params,
    });
    rmp_serde::to_vec_named(&frame).map_err(|e| ClientError::Encode(e.to_string()))
}

/// Decode a MessagePack `Frame::Response` body into a typed result.
///
/// [`RpcErrorObject`]s travel inside a 2xx response, so they are surfaced as
/// [`ClientError::Rpc`] here rather than as an HTTP status.
pub fn decode_response<C: RpcCall>(
    bytes: &[u8],
    expected: RequestId,
) -> Result<C::Result, ClientError> {
    let frame: Frame =
        rmp_serde::from_slice(bytes).map_err(|e| ClientError::Decode(e.to_string()))?;
    let Frame::Response(response) = frame else {
        return Err(ClientError::UnexpectedFrame);
    };
    if response.id != expected {
        return Err(ClientError::IdMismatch {
            expected,
            got: response.id,
        });
    }
    if let Some(error) = response.error {
        return Err(ClientError::Rpc {
            code: error.code,
            message: error.message,
        });
    }
    let result = response.result.ok_or(ClientError::EmptyResponse)?;
    serde_json::from_value(result).map_err(|e| ClientError::Decode(e.to_string()))
}

/// A bearer-authenticated MessagePack RPC client.
///
/// The base URL may be empty (same origin, i.e. web) or absolute (Android);
/// [`crate::config`] owns that distinction and this type never inspects it.
pub struct RpcClient {
    base: String,
    token: Option<String>,
    next_id: Cell<RequestId>,
}

impl RpcClient {
    /// A client for `base`, with no token yet. Request ids start at 1.
    pub fn new(base: impl Into<String>) -> Self {
        Self {
            base: config::normalize_base(&base.into()),
            token: None,
            next_id: Cell::new(1),
        }
    }

    /// Attach (or clear) the bearer token.
    pub fn with_token(mut self, token: Option<String>) -> Self {
        self.token = token.filter(|t| !t.is_empty());
        self
    }

    /// The normalized base URL (`""` for same origin).
    pub fn base(&self) -> &str {
        &self.base
    }

    /// The absolute/relative URL this client posts to.
    pub fn rpc_url(&self) -> String {
        config::rpc_url_for(&self.base)
    }

    fn take_id(&self) -> RequestId {
        let id = self.next_id.get();
        self.next_id.set(id.wrapping_add(1));
        id
    }

    /// Perform a typed request/response round trip.
    pub async fn call<C: RpcCall>(&self, params: C::Params) -> Result<C::Result, ClientError> {
        let id = self.take_id();
        let body = encode_request::<C>(id, &params)?;
        let (status, bytes) = post_msgpack(&self.rpc_url(), self.token.as_deref(), body).await?;
        match status {
            200..=299 => decode_response::<C>(&bytes, id),
            401 => Err(ClientError::Unauthorized),
            _ => Err(ClientError::Http {
                status,
                message: String::from_utf8_lossy(&bytes).into_owned(),
            }),
        }
    }
}

/// Generate one typed wrapper per client → server method.
///
/// The params/result types come from the [`RpcCall`] impl, so the wrapper list
/// stays a one-line-per-method mirror of the wire registry.
macro_rules! typed_calls {
    ($(
        $(#[$meta:meta])*
        $name:ident => $call:ty
    ),* $(,)?) => {
        impl RpcClient {
            $(
                $(#[$meta])*
                pub async fn $name(
                    &self,
                    params: <$call as RpcCall>::Params,
                ) -> Result<<$call as RpcCall>::Result, ClientError> {
                    self.call::<$call>(params).await
                }
            )*
        }
    };
}

typed_calls! {
    /// `system.ping`.
    ping => PingCall,
    /// `tasks.list`.
    tasks_list => TasksListCall,
    /// `tasks.get`.
    tasks_get => TasksGetCall,
    /// `tasks.cancel`.
    tasks_cancel => TasksCancelCall,
    /// `tasks.retry`.
    tasks_retry => TasksRetryCall,
    /// `tasks.start`.
    tasks_start => TasksStartCall,
    /// `tasks.start_oneshot`.
    tasks_start_oneshot => TasksStartOneshotCall,
    /// `catalog.list`.
    catalog_list => CatalogListCall,
    /// `catalog.get`.
    catalog_get => CatalogGetCall,
    /// `catalog.add`.
    catalog_add => CatalogAddCall,
    /// `catalog.update`.
    catalog_update => CatalogUpdateCall,
    /// `workflow.get`.
    workflow_get => WorkflowGetCall,
    /// `workflow.inspect`.
    workflow_inspect => WorkflowInspectCall,
    /// `workflow.create`.
    workflow_create => WorkflowCreateCall,
    /// `workflow.spawn`.
    workflow_spawn => WorkflowSpawnCall,
    /// `workflow.cancel`.
    workflow_cancel => WorkflowCancelCall,
    /// `workflow.retry`.
    workflow_retry => WorkflowRetryCall,
    /// `events.tail`.
    events_tail => EventsTailCall,
    /// `events.subscribe`.
    events_subscribe => EventsSubscribeCall,
    /// `agents.list`.
    agents_list => AgentsListCall,
    /// `agents.start`.
    agents_start => AgentsStartCall,
    /// `agents.input`.
    agents_input => AgentsInputCall,
    /// `agents.resize`.
    agents_resize => AgentsResizeCall,
    /// `agents.attach`.
    agents_attach => AgentsAttachCall,
    /// `agents.close`.
    agents_close => AgentsCloseCall,
    /// `agents.reply`.
    agents_reply => AgentsReplyCall,
    /// `providers.list`.
    providers_list => ProvidersListCall,
    /// `schedules.list`.
    schedules_list => SchedulesListCall,
    /// `schedules.upsert`.
    schedules_upsert => SchedulesUpsertCall,
    /// `schedules.delete`.
    schedules_delete => SchedulesDeleteCall,
    /// `notifications.list`.
    notifications_list => NotificationsListCall,
    /// `notifications.test`.
    notifications_test => NotificationsTestCall,
    /// `hooks.upsert`.
    hooks_upsert => HooksUpsertCall,
    /// `usage.stats`.
    usage_stats => UsageStatsCall,
    /// `repos.list`.
    repos_list => ReposListCall,
    /// `repos.add`.
    repos_add => ReposAddCall,
    /// `repos.remove`.
    repos_remove => ReposRemoveCall,
    /// `repos.activate`.
    repos_activate => ReposActivateCall,
}

/// `POST` a MessagePack body and return `(status, response bytes)`.
///
/// The browser implementation uses `fetch` with a bearer header. On the host
/// (unit tests, docs) there is no `fetch`, so it fails cleanly instead.
#[cfg(target_arch = "wasm32")]
async fn post_msgpack(
    url: &str,
    token: Option<&str>,
    body: Vec<u8>,
) -> Result<(u16, Vec<u8>), ClientError> {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;

    fn net(err: wasm_bindgen::JsValue) -> ClientError {
        ClientError::Network(format!("{err:?}"))
    }

    let window =
        web_sys::window().ok_or_else(|| ClientError::Network("no global window".into()))?;
    let headers = web_sys::Headers::new().map_err(net)?;
    headers
        .set("Content-Type", "application/x-msgpack")
        .map_err(net)?;
    headers
        .set("Accept", "application/x-msgpack")
        .map_err(net)?;
    if let Some(token) = token {
        headers
            .set("Authorization", &format!("Bearer {token}"))
            .map_err(net)?;
    }

    let init = web_sys::RequestInit::new();
    init.set_method("POST");
    init.set_headers(&headers);
    let array = js_sys::Uint8Array::from(body.as_slice());
    init.set_body(&wasm_bindgen::JsValue::from(array));

    let request = web_sys::Request::new_with_str_and_init(url, &init).map_err(net)?;
    let response = JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(net)?;
    let response: web_sys::Response = response
        .dyn_into()
        .map_err(|_| ClientError::Network("fetch did not return a Response".into()))?;
    let status = response.status();
    let buffer = JsFuture::from(response.array_buffer().map_err(net)?)
        .await
        .map_err(net)?;
    let bytes = js_sys::Uint8Array::new(&buffer).to_vec();
    Ok((status, bytes))
}

/// Host stub: no `fetch` outside the browser/WebView.
#[cfg(not(target_arch = "wasm32"))]
async fn post_msgpack(
    _url: &str,
    _token: Option<&str>,
    _body: Vec<u8>,
) -> Result<(u16, Vec<u8>), ClientError> {
    Err(ClientError::Network(
        "HTTP transport is only available on wasm32".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response_bytes(id: RequestId, result: Result<serde_json::Value, (i32, &str)>) -> Vec<u8> {
        let response = match result {
            Ok(value) => Response::ok(id, value),
            Err((code, message)) => Response {
                id,
                result: None,
                error: Some(RpcErrorObject {
                    code,
                    message: message.to_string(),
                    data: None,
                }),
            },
        };
        rmp_serde::to_vec_named(&Frame::Response(response)).unwrap()
    }

    #[test]
    fn request_round_trips_through_msgpack() {
        let bytes = encode_request::<PingCall>(7, &PingParams {}).unwrap();
        let frame: Frame = rmp_serde::from_slice(&bytes).unwrap();
        match frame {
            Frame::Request(request) => {
                assert_eq!(request.id, 7);
                assert_eq!(request.method, method::PING);
                assert_eq!(request.params, serde_json::json!({}));
            }
            other => panic!("expected a request frame, got {other:?}"),
        }
    }

    #[test]
    fn response_decodes_into_the_typed_result() {
        let bytes = response_bytes(3, Ok(serde_json::json!({ "pong": true, "cwd": "/tmp" })));
        let result: PingResult = decode_response::<PingCall>(&bytes, 3).unwrap();
        assert_eq!(
            result,
            PingResult {
                pong: true,
                cwd: "/tmp".into()
            }
        );
    }

    #[test]
    fn error_response_becomes_an_rpc_error() {
        let bytes = response_bytes(9, Err((-32602, "invalid params")));
        let error = decode_response::<PingCall>(&bytes, 9).unwrap_err();
        assert_eq!(
            error,
            ClientError::Rpc {
                code: -32602,
                message: "invalid params".into()
            }
        );
    }

    #[test]
    fn mismatched_id_is_rejected() {
        let bytes = response_bytes(1, Ok(serde_json::json!({ "pong": true, "cwd": "/" })));
        assert_eq!(
            decode_response::<PingCall>(&bytes, 2).unwrap_err(),
            ClientError::IdMismatch {
                expected: 2,
                got: 1
            }
        );
    }

    #[test]
    fn a_notification_is_not_a_response() {
        let frame = Frame::Notification(Notification {
            method: push::EVENT.to_string(),
            params: serde_json::json!({}),
        });
        let bytes = rmp_serde::to_vec_named(&frame).unwrap();
        assert_eq!(
            decode_response::<PingCall>(&bytes, 1).unwrap_err(),
            ClientError::UnexpectedFrame
        );
    }

    #[test]
    fn client_normalizes_the_base_url() {
        let same_origin = RpcClient::new("");
        assert_eq!(same_origin.base(), "");
        assert_eq!(same_origin.rpc_url(), "/rpc");

        let remote = RpcClient::new("http://192.168.1.10:8080/").with_token(Some("t".into()));
        assert_eq!(remote.base(), "http://192.168.1.10:8080");
        assert_eq!(remote.rpc_url(), "http://192.168.1.10:8080/rpc");
    }
}
