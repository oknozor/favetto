//! MessagePack RPC client over `POST {endpoint}/rpc`.
//!
//! The client links `favetto-wire`, so it produces and consumes the exact same
//! [`Frame`] bytes as the Unix-socket, WebSocket and browser transports: a
//! `Frame::Request` encoded with `rmp_serde::to_vec_named` is the request body,
//! and the response body is one `Frame::Response`. Authentication is a bearer
//! header.
//!
//! Every method has a typed wrapper generated from the [`RpcCall`] registry, so
//! a call site names the method once and the compiler checks both the params and
//! the decoded result. The wrappers deliberately mirror the web client's
//! (`web/src/client.rs`); this crate only depends on `favetto-wire`, never on
//! `web`.

#![allow(dead_code)] // The typed wrappers are the client API surface; views land in steps 03–09.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use favetto_wire::rpc::*;

use crate::config::{self, ClientConfig};

/// The request `Content-Type`/`Accept` the daemon expects.
pub const MSGPACK_CONTENT_TYPE: &str = "application/x-msgpack";

/// Anything that can go wrong between a typed call and its typed result.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClientError {
    /// The request params or the [`Frame`] could not be MessagePack-encoded.
    #[error("msgpack encode: {0}")]
    Encode(String),
    /// The response body could not be MessagePack-decoded into the result type.
    #[error("msgpack decode: {0}")]
    Decode(String),
    /// The request never reached the daemon (offline, DNS, timeout, TLS).
    #[error("network: {0}")]
    Network(String),
    /// The daemon answered with a non-2xx HTTP status other than 401.
    #[error("http error {status}: {message}")]
    Http { status: u16, message: String },
    /// Missing or rejected bearer token (HTTP 401, or the RPC unauthorized code).
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

impl ClientError {
    /// True when the daemon rejected the credentials, however it said so.
    pub fn is_unauthorized(&self) -> bool {
        match self {
            Self::Unauthorized => true,
            Self::Rpc { code, .. } => *code == error_code::UNAUTHORIZED,
            _ => false,
        }
    }
}

/// Encode a typed request into a standalone MessagePack `Frame::Request` body.
///
/// Exposed (rather than only used by [`RpcClient::call`]) so the wire encoding
/// has a unit test independent of any HTTP server.
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

/// Map a non-2xx HTTP status to an error, or `None` for 2xx bodies that should
/// be decoded as a frame.
///
/// A 401 is always [`ClientError::Unauthorized`] (the daemon's plain-text body
/// is not useful to the user), which lets the connection banner point at the
/// token file instead of rendering an HTTP dump.
fn status_error(status: u16, body: &[u8]) -> Option<ClientError> {
    match status {
        200..=299 => None,
        401 => Some(ClientError::Unauthorized),
        _ => Some(ClientError::Http {
            status,
            message: String::from_utf8_lossy(body).into_owned(),
        }),
    }
}

/// A bearer-authenticated MessagePack RPC client.
///
/// Cheap to clone (the underlying `reqwest::Client` and the request-id counter
/// are shared), so the runtime can hand one clone to each spawned call.
#[derive(Clone)]
pub struct RpcClient {
    http: reqwest::Client,
    rpc_url: String,
    token: Option<String>,
    next_id: Arc<AtomicU64>,
}

impl RpcClient {
    /// Build a client from the loaded settings.
    pub fn new(config: &ClientConfig) -> Result<Self, ClientError> {
        Self::from_parts(&config.endpoint, config.token.as_deref())
    }

    /// Build a client for an endpoint (normalized defensively) and token.
    pub fn from_parts(endpoint: &str, token: Option<&str>) -> Result<Self, ClientError> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(|err| ClientError::Network(format!("http client: {err}")))?;
        Ok(Self {
            http,
            rpc_url: config::rpc_url_for(endpoint),
            token: token.map(str::to_owned).filter(|t| !t.is_empty()),
            next_id: Arc::new(AtomicU64::new(1)),
        })
    }

    /// The absolute URL this client posts to.
    pub fn rpc_url(&self) -> &str {
        &self.rpc_url
    }

    fn take_id(&self) -> RequestId {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Perform a typed request/response round trip.
    pub async fn call<C: RpcCall>(&self, params: C::Params) -> Result<C::Result, ClientError> {
        let id = self.take_id();
        let body = encode_request::<C>(id, &params)?;
        let mut request = self
            .http
            .post(&self.rpc_url)
            .header(reqwest::header::CONTENT_TYPE, MSGPACK_CONTENT_TYPE)
            .header(reqwest::header::ACCEPT, MSGPACK_CONTENT_TYPE)
            .body(body);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }

        let response = request
            .send()
            .await
            .map_err(|err| ClientError::Network(err.to_string()))?;
        let status = response.status().as_u16();
        let bytes = response
            .bytes()
            .await
            .map_err(|err| ClientError::Network(err.to_string()))?;
        if let Some(error) = status_error(status, &bytes) {
            return Err(error);
        }
        decode_response::<C>(&bytes, id)
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
    /// `tasks.runs`.
    tasks_runs => TasksRunsCall,
    /// `tasks.events`.
    tasks_events => TasksEventsCall,
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
    /// `agents.list`.
    agents_list => AgentsListCall,
    /// `agents.history`.
    agents_history => AgentsHistoryCall,
    /// `agents.load`.
    agents_load => AgentsLoadCall,
    /// `agents.prompt`.
    agents_prompt => AgentsPromptCall,
    /// `agents.reply`.
    agents_reply => AgentsReplyCall,
    /// `agents.cancel`.
    agents_cancel => AgentsCancelCall,
    /// `agents.options`.
    agents_options => AgentsOptionsCall,
    /// `agents.set_option`.
    agents_set_option => AgentsSetOptionCall,
    /// `agents.set_mode`.
    agents_set_mode => AgentsSetModeCall,
    /// `agents.provider_sessions`.
    agents_provider_sessions => AgentsProviderSessionsCall,
    /// `agents.provider_resume`.
    agents_provider_resume => AgentsProviderResumeCall,
    /// `agents.provider_delete`.
    agents_provider_delete => AgentsProviderDeleteCall,
    /// `agents.authenticate`.
    agents_authenticate => AgentsAuthenticateCall,
    /// `agents.logout`.
    agents_logout => AgentsLogoutCall,
    /// `workflow.get`.
    workflow_get => WorkflowGetCall,
    /// `workflow.inspect`.
    workflow_inspect => WorkflowInspectCall,
    /// `workflow.cancel`.
    workflow_cancel => WorkflowCancelCall,
    /// `workflow.retry`.
    workflow_retry => WorkflowRetryCall,
    /// `schedules.list`.
    schedules_list => SchedulesListCall,
    /// `schedules.upsert`.
    schedules_upsert => SchedulesUpsertCall,
    /// `schedules.delete`.
    schedules_delete => SchedulesDeleteCall,
    /// `repos.list`.
    repos_list => ReposListCall,
    /// `repos.activate`.
    repos_activate => ReposActivateCall,
    /// `events.subscribe`.
    events_subscribe => EventsSubscribeCall,
}

#[cfg(test)]
mod tests {
    use super::*;
    use favetto_wire::model::InputReply;

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
        rmp_serde::to_vec_named(&Frame::Response(response)).expect("encode response")
    }

    #[test]
    fn request_round_trips_through_msgpack() {
        let bytes = encode_request::<PingCall>(7, &PingParams {}).expect("encode");
        let frame: Frame = rmp_serde::from_slice(&bytes).expect("decode frame");
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
        let result: PingResult = decode_response::<PingCall>(&bytes, 3).expect("decode");
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
        let error = decode_response::<PingCall>(&bytes, 9).expect_err("rpc error");
        assert_eq!(
            error,
            ClientError::Rpc {
                code: -32602,
                message: "invalid params".into()
            }
        );
        assert!(!error.is_unauthorized());
    }

    #[test]
    fn unauthorized_rpc_code_is_recognized() {
        let bytes = response_bytes(2, Err((error_code::UNAUTHORIZED, "denied")));
        let error = decode_response::<PingCall>(&bytes, 2).expect_err("unauthorized");
        assert!(error.is_unauthorized());
    }

    #[test]
    fn cancel_and_retry_requests_carry_the_task_id_and_method() {
        let id = uuid::Uuid::from_u128(42);
        let params = TaskIdParams { id };
        for (bytes, method_name) in [
            (
                encode_request::<TasksCancelCall>(1, &params).expect("encode cancel"),
                method::TASKS_CANCEL,
            ),
            (
                encode_request::<TasksRetryCall>(2, &params).expect("encode retry"),
                method::TASKS_RETRY,
            ),
        ] {
            let frame: Frame = rmp_serde::from_slice(&bytes).expect("decode frame");
            match frame {
                Frame::Request(request) => {
                    assert_eq!(request.method, method_name);
                    assert_eq!(request.params["id"], id.to_string());
                }
                other => panic!("expected a request frame, got {other:?}"),
            }
        }
    }

    #[test]
    fn repo_requests_use_the_registry_methods_and_optional_name() {
        let list = encode_request::<ReposListCall>(30, &EmptyParams {}).expect("encode list");
        let activate = encode_request::<ReposActivateCall>(
            31,
            &ReposActivateParams {
                name: Some("favetto".to_string()),
            },
        )
        .expect("encode activate");
        let clear = encode_request::<ReposActivateCall>(32, &ReposActivateParams { name: None })
            .expect("encode clear");

        let frame: Frame = rmp_serde::from_slice(&list).expect("decode list");
        match frame {
            Frame::Request(request) => assert_eq!(request.method, method::REPOS_LIST),
            other => panic!("expected a request frame, got {other:?}"),
        }

        let frame: Frame = rmp_serde::from_slice(&activate).expect("decode activate");
        match frame {
            Frame::Request(request) => {
                assert_eq!(request.method, method::REPOS_ACTIVATE);
                assert_eq!(request.params["name"], "favetto");
            }
            other => panic!("expected a request frame, got {other:?}"),
        }

        let frame: Frame = rmp_serde::from_slice(&clear).expect("decode clear");
        match frame {
            Frame::Request(request) => {
                assert_eq!(request.method, method::REPOS_ACTIVATE);
                assert!(request.params["name"].is_null());
            }
            other => panic!("expected a request frame, got {other:?}"),
        }
    }

    #[test]
    fn agent_history_and_load_requests_use_the_favetto_local_id() {
        let history =
            encode_request::<AgentsHistoryCall>(20, &AgentsHistoryParams { limit: Some(25) })
                .expect("encode history");
        let load = encode_request::<AgentsLoadCall>(
            21,
            &SessionIdParams {
                session_id: "local-past".to_string(),
            },
        )
        .expect("encode load");

        let frame: Frame = rmp_serde::from_slice(&history).expect("decode history");
        match frame {
            Frame::Request(request) => {
                assert_eq!(request.method, method::AGENTS_HISTORY);
                assert_eq!(request.params["limit"], 25);
                assert!(request.params.get("agent").is_none());
            }
            other => panic!("expected a request frame, got {other:?}"),
        }

        let frame: Frame = rmp_serde::from_slice(&load).expect("decode load");
        match frame {
            Frame::Request(request) => {
                assert_eq!(request.method, method::AGENTS_LOAD);
                // The routing key is the Favetto-local id, not a provider id.
                assert_eq!(request.params["session_id"], "local-past");
            }
            other => panic!("expected a request frame, got {other:?}"),
        }
    }

    #[test]
    fn workflow_requests_carry_the_root_or_task_id() {
        let root_id = uuid::Uuid::from_u128(7);
        let task_id = uuid::Uuid::from_u128(8);
        let graph = encode_request::<WorkflowGetCall>(1, &EmptyParams {}).expect("encode get");
        let inspect = encode_request::<WorkflowInspectCall>(2, &WorkflowInspectParams { root_id })
            .expect("encode inspect");
        let cancel = encode_request::<WorkflowCancelCall>(3, &WorkflowInspectParams { root_id })
            .expect("encode cancel");
        let retry = encode_request::<WorkflowRetryCall>(4, &WorkflowRetryParams { task_id })
            .expect("encode retry");

        for (bytes, method_name) in [
            (graph, method::WORKFLOW_GET),
            (inspect, method::WORKFLOW_INSPECT),
            (cancel, method::WORKFLOW_CANCEL),
            (retry, method::WORKFLOW_RETRY),
        ] {
            let frame: Frame = rmp_serde::from_slice(&bytes).expect("decode frame");
            match frame {
                Frame::Request(request) => assert_eq!(request.method, method_name),
                other => panic!("expected a request frame, got {other:?}"),
            }
        }
    }

    #[test]
    fn catalog_and_one_shot_requests_carry_their_typed_fields() {
        let start = encode_request::<TasksStartCall>(
            11,
            &TasksStartParams {
                name: "repo:favetto/triage".to_string(),
                input: Some(serde_json::json!({ "issue": 7, "note": "hi" })),
                interactive: None,
            },
        )
        .expect("encode start");
        let frame: Frame = rmp_serde::from_slice(&start).expect("decode frame");
        match frame {
            Frame::Request(request) => {
                assert_eq!(request.method, method::TASKS_START);
                assert_eq!(request.params["name"], "repo:favetto/triage");
                assert_eq!(request.params["input"]["issue"], 7);
            }
            other => panic!("expected a request frame, got {other:?}"),
        }

        let preview = encode_request::<CatalogGetCall>(
            12,
            &CatalogGetParams {
                name: "triage".to_string(),
            },
        )
        .expect("encode catalog get");
        let frame: Frame = rmp_serde::from_slice(&preview).expect("decode frame");
        match frame {
            Frame::Request(request) => {
                assert_eq!(request.method, method::CATALOG_GET);
                assert_eq!(request.params["name"], "triage");
            }
            other => panic!("expected a request frame, got {other:?}"),
        }

        let oneshot = encode_request::<TasksStartOneshotCall>(
            13,
            &StartOneshotParams {
                agent: Some("opencode".to_string()),
                cwd: Some("/code/favetto".to_string()),
                ..StartOneshotParams::default()
            },
        )
        .expect("encode oneshot");
        let frame: Frame = rmp_serde::from_slice(&oneshot).expect("decode frame");
        match frame {
            Frame::Request(request) => {
                assert_eq!(request.method, method::TASKS_START_ONESHOT);
                assert_eq!(request.params["agent"], "opencode");
                assert_eq!(request.params["cwd"], "/code/favetto");
            }
            other => panic!("expected a request frame, got {other:?}"),
        }
    }

    #[test]
    fn agent_reply_request_carries_the_typed_reply() {
        let params = AgentReplyParams {
            session_id: "sess-1".to_string(),
            request_id: "req-1".to_string(),
            reply: InputReply::Once,
        };
        let bytes = encode_request::<AgentsReplyCall>(4, &params).expect("encode");
        let frame: Frame = rmp_serde::from_slice(&bytes).expect("decode frame");
        match frame {
            Frame::Request(request) => {
                assert_eq!(request.id, 4);
                assert_eq!(request.method, method::AGENTS_REPLY);
                assert_eq!(request.params["session_id"], "sess-1");
                assert_eq!(request.params["request_id"], "req-1");
                // `InputReply` is internally tagged, so the reply nests under the
                // same field name.
                assert_eq!(
                    request.params["reply"],
                    serde_json::json!({ "reply": "once" })
                );
            }
            other => panic!("expected a request frame, got {other:?}"),
        }
    }

    #[test]
    fn agent_cancel_request_uses_the_favetto_local_session_id() {
        let params = AgentCancelParams {
            session_id: "local-live".to_string(),
        };
        let bytes = encode_request::<AgentsCancelCall>(5, &params).expect("encode");
        let frame: Frame = rmp_serde::from_slice(&bytes).expect("decode frame");
        match frame {
            Frame::Request(request) => {
                assert_eq!(request.id, 5);
                assert_eq!(request.method, method::AGENTS_CANCEL);
                assert_eq!(request.params["session_id"], "local-live");
            }
            other => panic!("expected a request frame, got {other:?}"),
        }
    }

    #[test]
    fn agent_prompt_request_preserves_an_unknown_command_verbatim() {
        let params = AgentPromptParams {
            session_id: "sess-1".to_string(),
            prompt: "/unknown arg with spaces".to_string(),
        };
        let bytes = encode_request::<AgentsPromptCall>(5, &params).expect("encode");
        let frame: Frame = rmp_serde::from_slice(&bytes).expect("decode frame");
        match frame {
            Frame::Request(request) => {
                assert_eq!(request.id, 5);
                assert_eq!(request.method, method::AGENTS_PROMPT);
                assert_eq!(request.params["session_id"], "sess-1");
                assert_eq!(request.params["prompt"], "/unknown arg with spaces");
            }
            other => panic!("expected a request frame, got {other:?}"),
        }
    }

    #[test]
    fn agent_load_response_carries_the_replayed_frame() {
        let bytes = response_bytes(
            22,
            Ok(serde_json::json!({
                "session": {
                    "id": "local-past",
                    "agent": "opencode",
                    "task_id": null,
                    "running": true
                },
                "data": "aGlzdG9yeQ==",
                "seq": 4
            })),
        );
        let result: AgentAttachResult =
            decode_response::<AgentsLoadCall>(&bytes, 22).expect("decode load");
        assert_eq!(result.session.id, "local-past");
        assert_eq!(result.data, "aGlzdG9yeQ==");
        assert_eq!(result.seq, 4);
    }

    #[test]
    fn mismatched_id_is_rejected() {
        let bytes = response_bytes(1, Ok(serde_json::json!({ "pong": true, "cwd": "/" })));
        assert_eq!(
            decode_response::<PingCall>(&bytes, 2).expect_err("id mismatch"),
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
        let bytes = rmp_serde::to_vec_named(&frame).expect("encode frame");
        assert_eq!(
            decode_response::<PingCall>(&bytes, 1).expect_err("not a response"),
            ClientError::UnexpectedFrame
        );
    }

    #[test]
    fn malformed_bytes_are_a_decode_error() {
        assert!(matches!(
            decode_response::<PingCall>(b"not msgpack", 1),
            Err(ClientError::Decode(_))
        ));
    }

    #[test]
    fn a_response_without_result_or_error_is_empty() {
        let response = Response {
            id: 5,
            result: None,
            error: None,
        };
        let bytes = rmp_serde::to_vec_named(&Frame::Response(response)).expect("encode");
        assert_eq!(
            decode_response::<PingCall>(&bytes, 5).expect_err("empty"),
            ClientError::EmptyResponse
        );
    }

    #[test]
    fn http_401_is_unauthorized() {
        let error = status_error(401, b"missing or invalid bearer token").expect("error");
        assert_eq!(error, ClientError::Unauthorized);
        assert!(error.is_unauthorized());
    }

    #[test]
    fn other_http_errors_carry_the_status_and_body() {
        let error = status_error(500, b"boom").expect("error");
        assert_eq!(
            error,
            ClientError::Http {
                status: 500,
                message: "boom".into()
            }
        );
        assert!(status_error(204, b"").is_none());
    }

    #[test]
    fn the_client_builds_the_rpc_url_from_any_endpoint_form() {
        let client = RpcClient::from_parts("127.0.0.1:7878/rpc", Some("t")).expect("client");
        assert_eq!(client.rpc_url(), "http://127.0.0.1:7878/rpc");
    }
}
