//! Request dispatch and the connection lifecycle shared by both transports.
//!
//! `serve_connection` is transport-agnostic: it takes a `Stream` of incoming frames
//! and a `Sink` for outgoing frames. The Unix-socket and WebSocket transports are
//! just different ways of producing those two halves, which is why local attach is
//! merely a special case of remote attach.

use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::Arc;

use base64::Engine as _;
use chrono::Utc;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

use favetto_core::model::{
    AgentCatalogEntry, AgentSessionInfo, EventKind, InputReply, Task, TaskStatus,
};
use favetto_core::rpc::{method, push, Frame, Notification, Request, Response, RpcError};
use favetto_core::wire::WireError;
use favetto_core::workflow::{
    WorkflowCreateResult, WorkflowInspect, WorkflowNodeRef, WorkflowState, WorkflowTaskView,
};

use crate::agents::{AgentContext, AgentEvent, Invocation};
use crate::db;
use crate::event_bus::ServerPush;
use crate::state::State;
use crate::tasks::needs_parts;

/// Per-connection set of attached agent sessions. Used to scope screen frames to
/// interested clients.
type Attached = Arc<Mutex<HashSet<String>>>;

/// Boxed, `Send` frame stream/sink handed over by a transport.
pub type BoxIn = Pin<Box<dyn Stream<Item = Result<Frame, WireError>> + Send>>;
pub type BoxOut = Pin<Box<dyn Sink<Frame, Error = WireError> + Send>>;

/// Drive one client connection until it closes.
///
/// - A writer task drains `out_rx` into the transport sink.
/// - A pusher task forwards bus broadcasts as `Frame::Notification`s.
/// - The read loop dispatches requests inline and handles `events.subscribe`
///   replay specially (it needs to emit more than one frame per request).
pub async fn serve_connection(state: Arc<State>, mut incoming: BoxIn, mut outgoing: BoxOut) {
    let (out_tx, mut out_rx) = mpsc::channel::<Frame>(256);

    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if outgoing.send(frame).await.is_err() {
                break;
            }
        }
    });

    let push_tx = out_tx.clone();
    let mut push_rx = state.bus.subscribe();
    let pusher = tokio::spawn(async move {
        loop {
            match push_rx.recv().await {
                Ok(push) => {
                    let frame = match push.into_notification() {
                        Ok(n) => Frame::Notification(n),
                        Err(e) => {
                            tracing::warn!(error = %e, "dropping push: serialization failed");
                            continue;
                        }
                    };
                    if push_tx.send(frame).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        n,
                        "client fell behind; events skipped (resume via last_event_id)"
                    );
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Forward agent screen frames to this connection, scoped to the sessions it
    // attached to. This bypasses the event bus (non-durable, high-volume).
    let subscribed: Attached = Arc::new(Mutex::new(HashSet::new()));
    let agent_forwarder = {
        let subscribed = subscribed.clone();
        let mut agent_rx = state.agents.subscribe();
        let agent_tx = out_tx.clone();
        tokio::spawn(async move {
            loop {
                match agent_rx.recv().await {
                    Ok(ev) => {
                        let sid = ev.session_id().to_string();
                        if !subscribed.lock().contains(&sid) {
                            continue;
                        }
                        let (method, params) = match ev {
                            AgentEvent::Output { data, .. } => (
                                push::AGENT_OUTPUT,
                                serde_json::json!({
                                    "session_id": sid,
                                    "data": base64::engine::general_purpose::STANDARD.encode(&data),
                                }),
                            ),
                            AgentEvent::Exit { code, .. } => (
                                push::AGENT_EXIT,
                                serde_json::json!({ "session_id": sid, "code": code }),
                            ),
                            AgentEvent::State { live, .. } => (
                                push::AGENT_STATE,
                                serde_json::json!({
                                    "session_id": sid,
                                    "activity": live.activity,
                                    "usage": live.usage,
                                }),
                            ),
                        };
                        let frame = Frame::Notification(Notification {
                            method: method.to_string(),
                            params,
                        });
                        if agent_tx.send(frame).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // Frames are self-contained, so a dropped frame is repaired
                        // by the next one; just note it.
                        tracing::debug!(n, "agent frame lagged; skipped");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    };

    while let Some(res) = incoming.next().await {
        let frame = match res {
            Ok(f) => f,
            Err(e) => {
                tracing::debug!(error = %e, "connection read error");
                break;
            }
        };
        match frame {
            Frame::Request(req) => {
                handle_request(&state, &out_tx, &subscribed, req).await;
            }
            Frame::Notification(_) | Frame::Response(_) => {}
        }
    }

    drop(out_tx);
    pusher.abort();
    agent_forwarder.abort();
    let _ = writer.await;
}

/// Route a single request. `events.subscribe` is handled here because it produces
/// an ack plus a variable number of replayed events; agent attach/close need the
/// per-connection subscription set.
async fn handle_request(
    state: &Arc<State>,
    out_tx: &mpsc::Sender<Frame>,
    subscribed: &Attached,
    req: Request,
) {
    let id = req.id;
    state.metrics.inc_rpc();

    if req.method == method::EVENTS_SUBSCRIBE {
        let _ = out_tx
            .send(Frame::Response(Response::ok(
                id,
                serde_json::json!({ "subscribed": true }),
            )))
            .await;

        // `events.subscribe` never fails on params: a malformed object is treated
        // as "no replay" so the subscription still succeeds.
        let p: EventsSubscribeParams = parse_params(&req.method, &req.params).unwrap_or_default();
        if let Some(last) = p.last_event_id {
            match db::events_after(&state.db, last, 500).await {
                Ok(events) => {
                    for ev in events {
                        let frame = match ev.into_notification() {
                            Ok(n) => Frame::Notification(n),
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    "dropping replayed event: serialization failed"
                                );
                                continue;
                            }
                        };
                        if out_tx.send(frame).await.is_err() {
                            break;
                        }
                    }
                }
                Err(e) => tracing::warn!(error = %e, "event replay failed"),
            }
        }
        return;
    }

    // `agents.start` spawns the PTY and immediately attaches, returning the current
    // full-screen frame so the client starts in sync.
    if req.method == method::AGENTS_START {
        let resp = match start_agent(state, &req.params).await {
            Ok(session) => match state.agents.attach(&session.id) {
                Ok((session, data)) => {
                    subscribed.lock().insert(session.id.clone());
                    Response::ok(
                        id,
                        serde_json::json!({
                            "session": session,
                            "data": base64::engine::general_purpose::STANDARD.encode(&data),
                        }),
                    )
                }
                Err(e) => Response::error(id, RpcError::Internal(e.to_string())),
            },
            Err(e) => Response::error(id, RpcError::InvalidParams(e.to_string())),
        };
        let _ = out_tx.send(Frame::Response(resp)).await;
        return;
    }

    // `tasks.start_oneshot` creates an inline task and opens its interactive
    // session; subscribe this connection so the Agent panel receives its frames.
    if req.method == method::TASKS_START_ONESHOT {
        let resp = match start_oneshot_task(state, &req.params).await {
            Ok(value) => {
                if let Some(sid) = value
                    .get("session")
                    .and_then(|s| s.get("id"))
                    .and_then(|v| v.as_str())
                {
                    subscribed.lock().insert(sid.to_string());
                }
                Response::ok(id, value)
            }
            Err(e) => Response::error(id, RpcError::Internal(e.to_string())),
        };
        let _ = out_tx.send(Frame::Response(resp)).await;
        return;
    }

    // `agents.attach` subscribes this connection; `agents.close` unsubscribes.
    if req.method == method::AGENTS_ATTACH {
        let resp = match parse_params::<SessionIdParams>(&req.method, &req.params) {
            Ok(p) => match state.agents.attach(&p.session_id) {
                Ok((session, data)) => {
                    subscribed.lock().insert(p.session_id);
                    Response::ok(
                        id,
                        serde_json::json!({
                            "session": session,
                            "data": base64::engine::general_purpose::STANDARD.encode(&data),
                        }),
                    )
                }
                Err(e) => Response::error(id, RpcError::Internal(e.to_string())),
            },
            Err(error) => Response::error(id, error),
        };
        let _ = out_tx.send(Frame::Response(resp)).await;
        return;
    }

    if req.method == method::AGENTS_CLOSE {
        let resp = match parse_params::<SessionIdParams>(&req.method, &req.params) {
            Ok(p) => {
                subscribed.lock().remove(&p.session_id);
                match state.agents.close(&p.session_id) {
                    Ok(()) => Response::ok(id, serde_json::json!({ "closed": p.session_id })),
                    Err(e) => Response::error(id, RpcError::Internal(e.to_string())),
                }
            }
            Err(error) => Response::error(id, error),
        };
        let _ = out_tx.send(Frame::Response(resp)).await;
        return;
    }

    let resp = dispatch(state, req).await;
    let _ = out_tx.send(Frame::Response(resp)).await;
}

/// Deserialize an optional field leniently: a missing or ill-typed value becomes
/// `None`, matching the `params.get(..).and_then(..)` chains these typed params
/// replace. Required fields deliberately do not use this.
fn lenient<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: DeserializeOwned,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| serde_json::from_value(value).ok()))
}

/// Deserialize request `params` into a typed struct, surfacing serde's message as
/// an `anyhow` error for the helper functions below.
fn parse_value<T: DeserializeOwned>(params: &serde_json::Value) -> anyhow::Result<T> {
    Ok(serde_json::from_value(params.clone())?)
}

/// Deserialize request `params` into the typed struct for `method`.
///
/// A malformed object is an [`RpcError::InvalidParams`] with a stable message.
fn parse_params<T: DeserializeOwned>(
    method: &str,
    params: &serde_json::Value,
) -> Result<T, RpcError> {
    parse_value(params)
        .map_err(|err| RpcError::InvalidParams(format!("invalid params for {method}: {err}")))
}

/// `tasks.list` params. `limit` defaults to 500 and caps at 2000.
#[derive(Debug, Default, Deserialize)]
struct TasksListParams {
    #[serde(default, deserialize_with = "lenient")]
    limit: Option<u64>,
}

/// `tasks.get` / `tasks.cancel` params.
#[derive(Debug, Deserialize)]
struct TaskIdParams {
    id: Uuid,
}

/// `workflow.inspect` params.
#[derive(Debug, Deserialize)]
struct WorkflowInspectParams {
    root_id: Uuid,
}

/// One node of a `workflow.create` request. `key` is local to the request and is
/// what sibling nodes reference from `depends_on`.
#[derive(Debug, Deserialize)]
struct WorkflowCreateTask {
    key: String,
    name: String,
    #[serde(default, deserialize_with = "lenient")]
    input: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "lenient")]
    depends_on: Option<Vec<String>>,
}

/// `workflow.create` params. `idempotency_key` scopes the per-node dedupe keys;
/// `root_id` optionally extends an existing root.
#[derive(Debug, Deserialize)]
struct WorkflowCreateParams {
    idempotency_key: String,
    #[serde(default, deserialize_with = "lenient")]
    root_id: Option<Uuid>,
    #[serde(default, deserialize_with = "lenient")]
    tasks: Option<Vec<WorkflowCreateTask>>,
}

/// `workflow.spawn` params.
#[derive(Debug, Deserialize)]
struct WorkflowSpawnParams {
    name: String,
    #[serde(default, deserialize_with = "lenient")]
    input: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "lenient")]
    root_id: Option<Uuid>,
    #[serde(default, deserialize_with = "lenient")]
    depends_on: Option<Vec<Uuid>>,
    #[serde(default, deserialize_with = "lenient")]
    dedupe_key: Option<String>,
}

/// `tasks.start` params. `input` defaults to JSON null; `interactive` defaults
/// to false (headless), so only the TUI opts into the live embedded TUI.
#[derive(Debug, Deserialize)]
struct TasksStartParams {
    name: String,
    #[serde(default, deserialize_with = "lenient")]
    input: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "lenient")]
    interactive: Option<bool>,
}

/// `catalog.get` params.
#[derive(Debug, Deserialize)]
struct CatalogGetParams {
    name: String,
}

/// `events.tail` params. `limit` defaults to 50 and caps at 1000.
#[derive(Debug, Default, Deserialize)]
struct EventsTailParams {
    #[serde(default, deserialize_with = "lenient")]
    limit: Option<u64>,
}

/// `events.subscribe` params. A malformed request degrades to "no replay".
#[derive(Debug, Default, Deserialize)]
struct EventsSubscribeParams {
    #[serde(default, deserialize_with = "lenient")]
    last_event_id: Option<i64>,
}

/// `notifications.list` params. `limit` defaults to 50 and caps at 1000.
#[derive(Debug, Default, Deserialize)]
struct NotificationsListParams {
    #[serde(default, deserialize_with = "lenient")]
    limit: Option<u64>,
}

/// `agents.attach` / `agents.close` params.
#[derive(Debug, Deserialize)]
struct SessionIdParams {
    session_id: String,
}

/// `agents.input` params.
#[derive(Debug, Deserialize)]
struct AgentInputParams {
    session_id: String,
    #[serde(default, deserialize_with = "lenient")]
    data: Option<String>,
}

/// `agents.resize` params. `rows`/`cols` default to 24/80.
#[derive(Debug, Deserialize)]
struct AgentResizeParams {
    session_id: String,
    #[serde(default, deserialize_with = "lenient")]
    rows: Option<u64>,
    #[serde(default, deserialize_with = "lenient")]
    cols: Option<u64>,
}

/// `agents.reply` params: answer the request identified by `request_id`.
#[derive(Debug, Deserialize)]
struct AgentReplyParams {
    session_id: String,
    request_id: String,
    reply: InputReply,
}

/// `providers.list` params.
#[derive(Debug, Default, Deserialize)]
struct ProvidersListParams {
    #[serde(default, deserialize_with = "lenient")]
    agent: Option<String>,
}

/// `schedules.delete` params.
#[derive(Debug, Deserialize)]
struct ScheduleDeleteParams {
    id: String,
}

/// `schedules.upsert` params.
#[derive(Debug, Deserialize)]
struct ScheduleUpsertParams {
    #[serde(default, deserialize_with = "lenient")]
    id: Option<String>,
    cron: String,
    task: String,
    #[serde(default, deserialize_with = "lenient")]
    input: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "lenient")]
    enabled: Option<bool>,
}

/// `notifications.test` params.
#[derive(Debug, Deserialize)]
struct NotificationTestParams {
    channel: String,
    #[serde(default, deserialize_with = "lenient")]
    config: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "lenient")]
    subject: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    body: Option<String>,
}

/// `hooks.upsert` params.
#[derive(Debug, Deserialize)]
struct HookUpsertParams {
    event: String,
    channel: String,
    #[serde(default, deserialize_with = "lenient")]
    config: Option<serde_json::Value>,
}

/// `catalog.add` params. `spawn_new_root` defaults to false, `prompt` to "".
#[derive(Debug, Deserialize)]
struct CatalogAddParams {
    name: String,
    #[serde(default, deserialize_with = "lenient")]
    agent: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    provider: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    model: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    cwd: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    schedule: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    needs: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    spawn: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    spawn_file: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    spawn_new_root: Option<bool>,
    #[serde(default, deserialize_with = "lenient")]
    prompt: Option<String>,
}

/// `catalog.update` params.
#[derive(Debug, Deserialize)]
struct CatalogUpdateParams {
    name: String,
    markdown: String,
}

/// `agents.start` params. `rows`/`cols` default to 24/80, `new` to false.
#[derive(Debug, Default, Deserialize)]
struct StartAgentParams {
    #[serde(default, deserialize_with = "lenient")]
    agent: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    task_id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    prompt: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    session_id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    rows: Option<u64>,
    #[serde(default, deserialize_with = "lenient")]
    cols: Option<u64>,
    #[serde(default, deserialize_with = "lenient")]
    new: Option<bool>,
    #[serde(default, deserialize_with = "lenient")]
    provider: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    model: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    cwd: Option<std::path::PathBuf>,
}

/// `tasks.start_oneshot` params. `rows`/`cols` default to 24/80.
#[derive(Debug, Default, Deserialize)]
struct StartOneshotParams {
    #[serde(default, deserialize_with = "lenient")]
    agent: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    provider: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    model: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    cwd: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    rows: Option<u64>,
    #[serde(default, deserialize_with = "lenient")]
    cols: Option<u64>,
}

/// Execute a request and build its response.
pub async fn dispatch(state: &Arc<State>, req: Request) -> Response {
    let id = req.id;
    match dispatch_method(state, &req).await {
        Ok(value) => Response::ok(id, value),
        Err(error) => Response::error(id, error),
    }
}

/// Resolve a method's typed params and produce its result value.
async fn dispatch_method(state: &Arc<State>, req: &Request) -> Result<serde_json::Value, RpcError> {
    match req.method.as_str() {
        method::PING => Ok(serde_json::json!({
            "pong": true,
            "cwd": std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        })),

        method::TASKS_LIST => {
            let p: TasksListParams = parse_params(&req.method, &req.params)?;
            let limit = p.limit.unwrap_or(500).min(2000) as i64;
            match db::list_tasks(&state.db, limit).await {
                Ok(tasks) => Ok(serde_json::json!(tasks)),
                Err(e) => Err(RpcError::Internal(e.to_string())),
            }
        }

        method::TASKS_GET => {
            let p: TaskIdParams = parse_params(&req.method, &req.params)?;
            match db::get_task(&state.db, p.id).await {
                Ok(Some(task)) => Ok(serde_json::json!(task)),
                Ok(None) => Err(RpcError::InvalidParams("task not found".to_string())),
                Err(e) => Err(RpcError::Internal(e.to_string())),
            }
        }

        method::TASKS_CANCEL => {
            let p: TaskIdParams = parse_params(&req.method, &req.params)?;
            match cancel_task(state, p.id).await {
                Ok(task) => Ok(serde_json::json!(task)),
                Err(e) => Err(RpcError::Internal(e.to_string())),
            }
        }

        method::TASKS_RETRY => {
            let p: TaskIdParams = parse_params(&req.method, &req.params)?;
            retry_task(state, p.id)
                .await
                .map(|task| serde_json::json!(task))
        }

        method::TASKS_START => {
            let p: TasksStartParams = parse_params(&req.method, &req.params)?;
            let input = p.input.unwrap_or(serde_json::Value::Null);
            let interactive = p.interactive.unwrap_or(false);
            match crate::executor::enqueue_task(state, p.name, input, None, interactive).await {
                Ok(task) => Ok(serde_json::json!(task)),
                Err(e) => Err(RpcError::Internal(e.to_string())),
            }
        }

        method::CATALOG_LIST => {
            let catalog = state.catalog.read().clone();
            let list: Vec<serde_json::Value> = catalog
                .iter()
                .map(|d| {
                    serde_json::json!({
                        "name": d.name,
                        "agent": d.agent,
                        "provider": d.provider,
                        "model": d.model,
                        "cwd": d.cwd,
                        "schedule": d.schedule,
                        "needs": d.needs,
                        "vars": d.vars,
                        "prompt": d.prompt,
                    })
                })
                .collect();
            Ok(serde_json::json!(list))
        }

        method::CATALOG_GET => {
            let p: CatalogGetParams = parse_params(&req.method, &req.params)?;
            let name = p.name;
            if let Err(e) = crate::tasks::validate_task_path(&name) {
                Err(RpcError::InvalidParams(e.to_string()))
            } else {
                let path = state.tasks_dir.join(format!("{name}.md"));
                let markdown = std::fs::read_to_string(&path).ok().or_else(|| {
                    state
                        .catalog
                        .read()
                        .iter()
                        .find(|d| d.name == name)
                        .map(crate::tasks::to_markdown)
                });
                match markdown {
                    Some(markdown) => Ok(serde_json::json!({ "markdown": markdown })),
                    None => Err(RpcError::InvalidParams(format!("unknown task '{name}'"))),
                }
            }
        }

        method::CATALOG_ADD => match add_catalog_task(state, &req.params).await {
            Ok(def) => Ok(serde_json::json!({
                "name": def.name,
                "agent": def.agent,
                "provider": def.provider,
                "model": def.model,
                "cwd": def.cwd,
                "schedule": def.schedule,
                "needs": def.needs,
                "vars": def.vars,
            })),
            Err(e) => Err(RpcError::Internal(e.to_string())),
        },

        method::CATALOG_UPDATE => match update_catalog_task(state, &req.params).await {
            Ok(()) => Ok(serde_json::json!({ "updated": true })),
            Err(e) => Err(RpcError::InvalidParams(e.to_string())),
        },

        method::WORKFLOW_GET => {
            let catalog = state.catalog.read().clone();
            Ok(serde_json::json!({
                "dot": crate::workflow::build_dot(&catalog),
                "path": crate::workflow::dot_path(&state.data_dir).to_string_lossy(),
                "graph": crate::workflow::build_graph(&catalog),
            }))
        }

        method::WORKFLOW_INSPECT => {
            let p: WorkflowInspectParams = parse_params(&req.method, &req.params)?;
            inspect_workflow(state, p.root_id)
                .await
                .map(|view| serde_json::json!(view))
        }

        method::WORKFLOW_CREATE => {
            let p: WorkflowCreateParams = parse_params(&req.method, &req.params)?;
            create_workflow(state, p)
                .await
                .map(|result| serde_json::json!(result))
        }

        method::WORKFLOW_SPAWN => {
            let p: WorkflowSpawnParams = parse_params(&req.method, &req.params)?;
            spawn_workflow(state, p)
                .await
                .map(|task| serde_json::json!(task))
        }

        method::EVENTS_TAIL => {
            let p: EventsTailParams = parse_params(&req.method, &req.params)?;
            let limit = p.limit.unwrap_or(50).min(1000) as i64;
            match db::tail_events(&state.db, limit).await {
                Ok(events) => Ok(serde_json::json!(events)),
                Err(e) => Err(RpcError::Internal(e.to_string())),
            }
        }

        method::AGENTS_LIST => {
            let sessions = state.agents.sessions();
            let default = state
                .registry
                .default_agent()
                .map(|agent| agent.descriptor().id.clone());
            let mut entries: Vec<AgentCatalogEntry> = state
                .registry
                .descriptors()
                .iter()
                .map(|d| AgentCatalogEntry {
                    name: d.id.clone(),
                    display_name: d.name.clone(),
                    command: d.command.clone(),
                    default: default.as_deref() == Some(d.id.as_str()),
                    available: d.available,
                    capabilities: d.capabilities,
                    sessions: sessions
                        .iter()
                        .filter(|s| s.agent == d.id)
                        .cloned()
                        .collect(),
                })
                .collect();
            // Surface sessions whose configured agent was later removed.
            for s in &sessions {
                if !entries.iter().any(|e| e.name == s.agent) {
                    entries.push(AgentCatalogEntry {
                        name: s.agent.clone(),
                        display_name: String::new(),
                        command: String::new(),
                        default: false,
                        available: false,
                        capabilities: Default::default(),
                        sessions: vec![s.clone()],
                    });
                }
            }
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(serde_json::json!(entries))
        }

        method::PROVIDERS_LIST => {
            let p: ProvidersListParams = parse_params(&req.method, &req.params)?;
            match list_providers(state, p.agent.as_deref()).await {
                Ok(providers) => Ok(serde_json::json!({ "providers": providers })),
                Err(e) => Err(RpcError::Internal(e.to_string())),
            }
        }

        method::AGENTS_INPUT => {
            let p: AgentInputParams = parse_params(&req.method, &req.params)?;
            let data = p
                .data
                .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
                .unwrap_or_default();
            match state.agents.input(&p.session_id, &data) {
                Ok(()) => Ok(serde_json::json!({ "bytes": data.len() })),
                Err(e) => Err(RpcError::Internal(e.to_string())),
            }
        }

        method::AGENTS_RESIZE => {
            let p: AgentResizeParams = parse_params(&req.method, &req.params)?;
            let rows = p.rows.unwrap_or(24) as u16;
            let cols = p.cols.unwrap_or(80) as u16;
            match state.agents.resize(&p.session_id, rows, cols) {
                Ok(()) => Ok(serde_json::json!({ "rows": rows, "cols": cols })),
                Err(e) => Err(RpcError::Internal(e.to_string())),
            }
        }

        method::AGENTS_REPLY => {
            let p: AgentReplyParams = parse_params(&req.method, &req.params)?;
            match state
                .agents
                .reply(&p.session_id, &p.request_id, p.reply)
                .await
            {
                Ok(()) => Ok(serde_json::json!({ "replied": true })),
                Err(e) => Err(RpcError::Internal(e.to_string())),
            }
        }

        method::SCHEDULES_LIST => match db::list_schedules(&state.db).await {
            Ok(schedules) => Ok(serde_json::json!(schedules)),
            Err(e) => Err(RpcError::Internal(e.to_string())),
        },

        method::SCHEDULES_UPSERT => match upsert_schedule(state, &req.params).await {
            Ok(schedule) => Ok(serde_json::json!(schedule)),
            Err(e) => Err(RpcError::Internal(e.to_string())),
        },

        method::SCHEDULES_DELETE => {
            let p: ScheduleDeleteParams = parse_params(&req.method, &req.params)?;
            match delete_schedule(state, &p.id).await {
                Ok(()) => Ok(serde_json::json!({ "deleted": p.id })),
                Err(e) => Err(RpcError::Internal(e.to_string())),
            }
        }

        method::NOTIFICATIONS_LIST => {
            let p: NotificationsListParams = parse_params(&req.method, &req.params)?;
            let limit = p.limit.unwrap_or(50).min(1000) as i64;
            match db::list_notifications(&state.db, limit).await {
                Ok(notifications) => Ok(serde_json::json!(notifications)),
                Err(e) => Err(RpcError::Internal(e.to_string())),
            }
        }

        method::NOTIFICATIONS_TEST => {
            let p: NotificationTestParams = parse_params(&req.method, &req.params)?;
            let config = p.config.unwrap_or(serde_json::Value::Null);
            let subject = p.subject.as_deref().unwrap_or("test");
            let body = p.body.as_deref().unwrap_or("test notification");
            crate::notify::send(state, &p.channel, &config, subject, body).await;
            Ok(serde_json::json!({ "sent": true }))
        }

        method::HOOKS_UPSERT => match upsert_hook(state, &req.params) {
            Ok(()) => Ok(serde_json::json!({ "added": true })),
            Err(e) => Err(RpcError::InvalidParams(e.to_string())),
        },

        _ => Err(RpcError::MethodNotFound(format!(
            "unknown method: {}",
            req.method
        ))),
    }
}

/// Build the runtime workflow view for `root_id` from the DB and the catalog's
/// `needs` declarations.
async fn inspect_workflow(state: &State, root_id: Uuid) -> Result<WorkflowInspect, RpcError> {
    let tasks = db::list_root_tasks(&state.db, root_id)
        .await
        .map_err(|e| RpcError::Internal(e.to_string()))?;
    if tasks.is_empty() {
        return Err(RpcError::InvalidParams(
            "workflow root not found".to_string(),
        ));
    }

    // `needs` by task name, copied out so no lock guard is held across await.
    let needs_of: HashMap<String, String> = {
        let catalog = state.catalog.read();
        catalog
            .iter()
            .filter_map(|d| d.needs.as_ref().map(|n| (d.name.clone(), n.clone())))
            .collect()
    };

    // Names with at least one non-terminal instance in this root: the exact set
    // `db::list_active_tasks_in_root` treats as an unmet dependency.
    let active: HashSet<&str> = tasks
        .iter()
        .filter(|t| {
            matches!(
                t.status,
                TaskStatus::Pending | TaskStatus::Running | TaskStatus::AwaitingInput
            )
        })
        .map(|t| t.name.as_str())
        .collect();

    // Runtime per-instance dependencies (from `workflow.create`/`spawn`), keyed
    // by dependent id. A missing predecessor is simply absent from `active_ids`,
    // so it never blocks.
    let active_ids: HashSet<Uuid> = tasks
        .iter()
        .filter(|t| {
            matches!(
                t.status,
                TaskStatus::Pending | TaskStatus::Running | TaskStatus::AwaitingInput
            )
        })
        .map(|t| t.id)
        .collect();
    let mut runtime_deps: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for (task_id, depends_on) in db::list_root_dependencies(&state.db, root_id)
        .await
        .map_err(|e| RpcError::Internal(e.to_string()))?
    {
        runtime_deps.entry(task_id).or_default().push(depends_on);
    }

    let root_task = tasks
        .iter()
        .find(|t| t.id == root_id)
        .map(|t| t.name.clone())
        .unwrap_or_else(|| tasks[0].name.clone());

    let mut views: Vec<WorkflowTaskView> = Vec::with_capacity(tasks.len());
    let mut ready = Vec::new();
    let mut running = Vec::new();
    let mut failed = Vec::new();
    let mut blocked = Vec::new();

    for task in &tasks {
        let catalog_blocked = needs_of
            .get(&task.name)
            .map(|needs| {
                let (source, _) = needs_parts(needs);
                active.contains(source)
            })
            .unwrap_or(false);
        let runtime_blocked = runtime_deps
            .get(&task.id)
            .map(|predecessors| predecessors.iter().any(|p| active_ids.contains(p)))
            .unwrap_or(false);
        let is_blocked = task.status == TaskStatus::Pending && (catalog_blocked || runtime_blocked);

        match task.status {
            TaskStatus::Pending if is_blocked => blocked.push(task.id),
            TaskStatus::Pending => ready.push(task.id),
            TaskStatus::Running | TaskStatus::AwaitingInput => running.push(task.id),
            TaskStatus::Failed => failed.push(task.id),
            TaskStatus::Succeeded | TaskStatus::Cancelled => {}
        }

        views.push(WorkflowTaskView {
            id: task.id,
            name: task.name.clone(),
            status: task.status,
            attempt: task.attempt,
            summary: task.error.clone(),
        });
    }

    let overall = workflow_state(&views);
    Ok(WorkflowInspect {
        root_id,
        root_task,
        state: overall,
        tasks: views,
        ready,
        running,
        failed,
        blocked,
    })
}

/// Validate a `workflow.create` request before any row is inserted: non-empty,
/// unique, catalog-known names, in-request `depends_on` keys, and an acyclic
/// graph.
fn validate_create_dag(state: &State, tasks: &[WorkflowCreateTask]) -> Result<(), RpcError> {
    if tasks.is_empty() {
        return Err(RpcError::InvalidParams(
            "workflow.create requires at least one task".to_string(),
        ));
    }

    let catalog_names: HashSet<String> = {
        let catalog = state.catalog.read();
        catalog.iter().map(|d| d.name.clone()).collect()
    };

    let mut keys: HashSet<&str> = HashSet::new();
    for spec in tasks {
        if spec.key.trim().is_empty() {
            return Err(RpcError::InvalidParams(
                "workflow.create task keys must be non-empty".to_string(),
            ));
        }
        if !keys.insert(spec.key.as_str()) {
            return Err(RpcError::InvalidParams(format!(
                "workflow.create duplicate task key '{}'",
                spec.key
            )));
        }
        if !catalog_names.contains(&spec.name) {
            return Err(RpcError::InvalidParams(format!(
                "unknown task '{}'",
                spec.name
            )));
        }
    }

    // Every dependency must name a key in this request, and the graph must be a
    // DAG. Kahn's algorithm: repeatedly drop nodes with no incoming edges.
    let mut indegree: HashMap<&str, usize> = tasks.iter().map(|s| (s.key.as_str(), 0)).collect();
    let mut edges: HashMap<&str, Vec<&str>> = HashMap::new();
    for spec in tasks {
        for dep in spec.depends_on.as_deref().unwrap_or_default() {
            if !keys.contains(dep.as_str()) {
                return Err(RpcError::InvalidParams(format!(
                    "workflow.create task '{}' depends on unknown key '{dep}'",
                    spec.key
                )));
            }
            edges
                .entry(dep.as_str())
                .or_default()
                .push(spec.key.as_str());
            *indegree.get_mut(spec.key.as_str()).expect("key seeded") += 1;
        }
    }

    let mut ready: Vec<&str> = indegree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(key, _)| *key)
        .collect();
    let mut visited = 0usize;
    while let Some(key) = ready.pop() {
        visited += 1;
        if let Some(successors) = edges.get(key) {
            for successor in successors {
                let degree = indegree.get_mut(successor).expect("key seeded");
                *degree -= 1;
                if *degree == 0 {
                    ready.push(successor);
                }
            }
        }
    }
    if visited != tasks.len() {
        return Err(RpcError::InvalidParams(
            "workflow.create task graph contains a cycle".to_string(),
        ));
    }

    Ok(())
}

/// `workflow.create`: build a validated, idempotent runtime DAG of catalog tasks
/// with per-instance dependencies.
async fn create_workflow(
    state: &Arc<State>,
    params: WorkflowCreateParams,
) -> Result<WorkflowCreateResult, RpcError> {
    let specs = params.tasks.unwrap_or_default();
    validate_create_dag(state, &specs)?;

    if let Some(root_id) = params.root_id {
        if db::get_task(&state.db, root_id)
            .await
            .map_err(|e| RpcError::Internal(e.to_string()))?
            .is_none()
        {
            return Err(RpcError::InvalidParams(format!(
                "workflow root '{root_id}' not found"
            )));
        }
    }

    // Resolve every node's id up front so dependency rows can be written when
    // each task is inserted. A re-submission finds the row by its per-node dedupe
    // key and reuses the canonical id; everything else gets a fresh one.
    let mut ids: HashMap<String, Uuid> = HashMap::new();
    let mut existing: HashSet<Uuid> = HashSet::new();
    for spec in &specs {
        let dedupe_key = format!("workflow:{}:{}", params.idempotency_key, spec.key);
        match db::get_task_by_dedupe_key(&state.db, &dedupe_key)
            .await
            .map_err(|e| RpcError::Internal(e.to_string()))?
        {
            Some(task) => {
                existing.insert(task.id);
                ids.insert(spec.key.clone(), task.id);
            }
            None => {
                ids.insert(spec.key.clone(), Uuid::new_v4());
            }
        }
    }

    // The root is the caller's root, or the first task (which becomes its own
    // root, matching `Task::root_or_self`).
    let root_id = match params.root_id {
        Some(root_id) => root_id,
        None => *ids.get(&specs[0].key).expect("every key resolved"),
    };

    for spec in &specs {
        let id = ids[&spec.key];
        let depends_on: Vec<Uuid> = spec
            .depends_on
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|key| ids[key])
            .collect();
        if existing.contains(&id) {
            // Already inserted by an earlier submission; just make sure its
            // dependency edges are present (idempotent).
            let now = Utc::now();
            for predecessor in &depends_on {
                db::insert_dependency(&state.db, id, *predecessor, now)
                    .await
                    .map_err(|e| RpcError::Internal(e.to_string()))?;
            }
            continue;
        }
        let dedupe_key = format!("workflow:{}:{}", params.idempotency_key, spec.key);
        let parent_id = if id == root_id { None } else { Some(root_id) };
        crate::executor::enqueue_dynamic(
            state,
            crate::executor::DynamicNode {
                id,
                name: spec.name.clone(),
                input: spec.input.clone().unwrap_or(serde_json::Value::Null),
                dedupe_key: Some(dedupe_key),
                root_id,
                parent_id,
                depends_on,
            },
        )
        .await
        .map_err(|e| RpcError::Internal(e.to_string()))?;
    }

    let tasks = specs
        .iter()
        .map(|spec| WorkflowNodeRef {
            key: spec.key.clone(),
            id: ids[&spec.key],
            name: spec.name.clone(),
        })
        .collect();

    Ok(WorkflowCreateResult { root_id, tasks })
}

/// `workflow.spawn`: add one runtime task to an existing root (or as its own
/// root) with per-instance dependencies on existing task ids.
async fn spawn_workflow(state: &Arc<State>, params: WorkflowSpawnParams) -> Result<Task, RpcError> {
    let catalog_names: HashSet<String> = {
        let catalog = state.catalog.read();
        catalog.iter().map(|d| d.name.clone()).collect()
    };
    if !catalog_names.contains(&params.name) {
        return Err(RpcError::InvalidParams(format!(
            "unknown task '{}'",
            params.name
        )));
    }

    if let Some(root_id) = params.root_id {
        if db::get_task(&state.db, root_id)
            .await
            .map_err(|e| RpcError::Internal(e.to_string()))?
            .is_none()
        {
            return Err(RpcError::InvalidParams(format!(
                "workflow root '{root_id}' not found"
            )));
        }
    }

    let depends_on = params.depends_on.unwrap_or_default();
    for predecessor in &depends_on {
        let task = db::get_task(&state.db, *predecessor)
            .await
            .map_err(|e| RpcError::Internal(e.to_string()))?
            .ok_or_else(|| {
                RpcError::InvalidParams(format!("dependency task '{predecessor}' not found"))
            })?;
        if let Some(root_id) = params.root_id {
            if task.root_or_self() != root_id {
                return Err(RpcError::InvalidParams(format!(
                    "dependency task '{predecessor}' does not belong to root '{root_id}'"
                )));
            }
        }
    }

    crate::executor::spawn_dynamic(
        state,
        params.name,
        params.input.unwrap_or(serde_json::Value::Null),
        params.root_id,
        &depends_on,
        params.dedupe_key,
    )
    .await
    .map_err(|e| RpcError::Internal(e.to_string()))
}

/// Derive the root's overall state: still `running` while any task is
/// non-terminal, otherwise the worst terminal outcome.
fn workflow_state(tasks: &[WorkflowTaskView]) -> WorkflowState {
    use TaskStatus::*;
    if tasks
        .iter()
        .any(|t| matches!(t.status, Pending | Running | AwaitingInput))
    {
        WorkflowState::Running
    } else if tasks.iter().any(|t| t.status == Failed) {
        WorkflowState::Failed
    } else if tasks.iter().any(|t| t.status == Cancelled) {
        WorkflowState::Cancelled
    } else {
        WorkflowState::Succeeded
    }
}

/// Resolve the requested (or default) agent and spawn a PTY session.
///
/// Task runs are headless (their PTY carries machine output such as JSON), so
/// once the run has exited and a session id is known — from the request, the task
/// row, or the exited run — the agent's `resume_args` open a real interactive TUI.
/// A live interactive session is attached directly. A headless run still in
/// flight (or an agent that cannot resume) keeps its retained PTY: a finished run
/// is replayed and a still-running run is attached read-only
/// (`AgentSessionInfo::headless` tells the client not to forward keystrokes).
async fn start_agent(
    state: &Arc<State>,
    params: &serde_json::Value,
) -> anyhow::Result<AgentSessionInfo> {
    let StartAgentParams {
        agent: requested,
        task_id,
        prompt,
        session_id,
        rows,
        cols,
        new: force_new,
        provider: requested_provider,
        model: requested_model,
        cwd,
    } = parse_value(params)?;
    let rows = rows.unwrap_or(24) as u16;
    let cols = cols.unwrap_or(80) as u16;
    let force_new = force_new.unwrap_or(false);

    // Resolve the agent: a request wins over the configured default.
    let name = requested
        .or_else(|| state.config.agent.default.clone())
        .ok_or_else(|| {
            anyhow::anyhow!("no agent given and no default configured ([agent].default)")
        })?;
    let agent = state.registry.get_checked(&name)?;

    // A catalog task's prompt is a template over its collected `input`. Render it
    // here so a freshly seeded interactive session sees the same text a headless
    // run would, instead of raw `{{ input.* }}` placeholders. Pick up the task's
    // `provider`/`model` too, so the panel session matches the run instead of
    // silently falling back to the agent default. The request wins; the task
    // definition fills in whatever it didn't send.
    let (prompt, task_provider, task_model) = match task_id.as_deref() {
        Some(tid) => match task_and_definition(state, tid).await {
            Some((task, def)) => {
                let rendered = crate::executor::render_task_prompt(&def, &task);
                (
                    if rendered.is_empty() {
                        prompt
                    } else {
                        Some(rendered)
                    },
                    def.provider,
                    def.model,
                )
            }
            // Fall back to the caller-provided prompt when the task (or its
            // definition) can't be resolved.
            None => (prompt, None, None),
        },
        None => (prompt, None, None),
    };
    let provider = requested_provider.or(task_provider);
    let model = requested_model.or(task_model);

    let mut ctx = AgentContext {
        cwd,
        session_id: session_id.clone(),
        rows,
        cols,
        ..Default::default()
    };

    if !force_new {
        let live = task_id
            .as_deref()
            .and_then(|tid| state.agents.find_latest_by_task(tid));

        // Attach directly to a live interactive TUI already bound to the task, or
        // to a headless run that is blocked waiting for the user so keystrokes
        // reach the live PTY.
        if let Some(existing) = &live {
            if should_attach_to_live(existing) {
                return Ok(existing.clone());
            }
        }

        // A headless run that is still in flight owns its session file: resuming
        // it here would race two writers on the same agent session. Attach the
        // retained PTY read-only until it exits (or blocks on the user, above).
        if let Some(existing) = &live {
            if existing.running && existing.headless {
                return Ok(existing.clone());
            }
        }

        // Otherwise resume the agent's own session, never the headless run's PTY
        // (whose screen is machine output). The id comes from the request, the
        // task row, or an exited run whose session id was captured (the row write
        // may not have landed yet).
        if agent.capabilities().resume {
            let sid = match session_id.clone() {
                Some(sid) => Some(sid),
                None => match persisted_session_id(state, task_id.as_deref()).await {
                    Some(sid) => Some(sid),
                    None => live.as_ref().and_then(|s| s.session_id.clone()),
                },
            };
            if let Some(sid) = sid {
                // A headless run lives in the task's worktree, and some agents
                // (pi) scope their session store to the project directory, so the
                // run's directory is authoritative when reopening its session.
                // This overrides any base `cwd` the client sent (the task's repo
                // root, not the worktree) and recreates the worktree if retention
                // has already reclaimed it.
                if let Some(tid) = task_id.as_deref().and_then(|tid| Uuid::parse_str(tid).ok()) {
                    if let Some(cwd) = crate::executor::resume_cwd(state, tid).await {
                        ctx.cwd = Some(cwd);
                    }
                }
                return state
                    .agents
                    .start(&name, agent, task_id, Invocation::Resume(&sid), ctx);
            }
        }

        // Any other retained PTY bound to the task is shown in the panel rather
        // than seeding a duplicate run: a finished run's final screen is replayed,
        // and a still-running headless run is attached read-only (its PTY drives
        // itself, so the client must not forward keystrokes). The session info's
        // `headless` flag tells the client which mode to use.
        if let Some(existing) = &live {
            return Ok(existing.clone());
        }
    }

    let invocation = Invocation::Interactive {
        prompt: prompt.as_deref(),
        provider: provider.as_deref(),
        model: model.as_deref(),
    };
    // Let the agent create/seed its own session on a managed transport before the
    // PTY is spawned (opencode's managed server). A failure degrades to the old
    // prompt-on-the-command-line launch.
    let _ = agent.prepare_launch(&invocation, &mut ctx).await;
    state.agents.start(&name, agent, task_id, invocation, ctx)
}

/// Whether the daemon should reattach to an existing session rather than launch
/// a fresh one: any live interactive TUI, or a live headless run that is blocked
/// waiting for the user (so keystrokes can answer the prompt).
fn should_attach_to_live(info: &AgentSessionInfo) -> bool {
    info.running && (!info.headless || info.awaiting_input.is_some())
}

/// The agent session id persisted on a task row, if any.
async fn persisted_session_id(state: &Arc<State>, task_id: Option<&str>) -> Option<String> {
    let uuid = Uuid::parse_str(task_id?).ok()?;
    db::get_task(&state.db, uuid)
        .await
        .ok()
        .flatten()
        .and_then(|t| t.session_id)
}

/// The persisted task row plus the catalog definition backing it, by task id.
/// `None` when the id is malformed or either side cannot be resolved.
async fn task_and_definition(
    state: &Arc<State>,
    task_id: &str,
) -> Option<(Task, crate::tasks::TaskDef)> {
    let id = Uuid::parse_str(task_id).ok()?;
    let task = db::get_task(&state.db, id).await.ok().flatten()?;
    let def = state
        .catalog
        .read()
        .iter()
        .find(|d| d.name == task.name)
        .cloned()?;
    Some((task, def))
}

/// Providers and models for the requested (or default) agent, cached per agent
/// for the daemon's lifetime. Returns an empty list for an unknown agent or one
/// without a provider source.
async fn list_providers(
    state: &Arc<State>,
    requested: Option<&str>,
) -> anyhow::Result<Vec<favetto_providers::Provider>> {
    let agent = match requested {
        Some(name) => state.registry.get(name),
        None => state.registry.default_agent(),
    };
    let Some(agent) = agent else {
        return Ok(Vec::new());
    };
    let Some(source) = agent.provider_source() else {
        return Ok(Vec::new());
    };
    let key = agent.descriptor().id.clone();

    {
        let cache = state.providers_cache.lock().await;
        if let Some(providers) = cache.get(&key) {
            return Ok(providers.clone());
        }
    }

    let client = reqwest::Client::new();
    let providers = source.fetch(&client).await?;

    let mut cache = state.providers_cache.lock().await;
    cache.insert(key, providers.clone());
    Ok(providers)
}

/// Start a one-shot task: an inline (non-catalog) task whose work is an
/// interactive agent session the user drives from the Agent panel. Emits the
/// normal task lifecycle and finishes the task when the session exits.
async fn start_oneshot_task(
    state: &Arc<State>,
    params: &serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let StartOneshotParams {
        agent: requested,
        provider,
        model,
        cwd,
        rows,
        cols,
    } = parse_value(params)?;
    let cwd = cwd
        .map(|cwd| cwd.trim().to_string())
        .filter(|cwd| !cwd.is_empty());
    let rows = rows.unwrap_or(24) as u16;
    let cols = cols.unwrap_or(80) as u16;

    let name = requested
        .or_else(|| state.config.agent.default.clone())
        .ok_or_else(|| {
            anyhow::anyhow!("no agent given and no default configured ([agent].default)")
        })?;
    let agent = state.registry.get_checked(&name)?;
    let cwd_path = cwd.as_ref().map(std::path::PathBuf::from);
    // Keep a directory for the session-title lookup after `cwd_path` is moved
    // into the launch context below.
    let title_cwd = cwd_path
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    let task = Task {
        id: Uuid::new_v4(),
        name: "one-shot".to_string(),
        status: TaskStatus::Running,
        attempt: 0,
        input: serde_json::json!({
            "oneshot": true,
            "agent": name.clone(),
            "provider": provider.clone(),
            "model": model.clone(),
            "cwd": cwd_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
        }),
        output: None,
        dedupe_key: None,
        created_at: Utc::now(),
        started_at: Some(Utc::now()),
        finished_at: None,
        error: None,
        failure: None,
        session_id: None,
        session_title: None,
        parent_id: None,
        root_id: None,
        interactive: false,
    };
    db::insert_task(&state.db, &task).await?;
    state.metrics.inc_tasks();
    state
        .bus
        .publish(ServerPush::TaskUpdated(Box::new(task.summary())));
    state
        .emit_event(
            EventKind::TaskIdle,
            serde_json::json!({ "name": task.name, "task_id": task.id }),
        )
        .await;
    state
        .emit_event(
            EventKind::TaskStarted,
            serde_json::json!({ "name": task.name, "task_id": task.id }),
        )
        .await;

    let mut ctx = AgentContext {
        cwd: cwd_path,
        provider: provider.clone(),
        model: model.clone(),
        rows,
        cols,
        ..Default::default()
    };
    let invocation = Invocation::Interactive {
        prompt: None,
        provider: provider.as_deref(),
        model: model.as_deref(),
    };
    let _ = agent.prepare_launch(&invocation, &mut ctx).await;
    let info = state.agents.start(
        &name,
        agent.clone(),
        Some(task.id.to_string()),
        invocation,
        ctx,
    )?;

    // Best effort: some interactive CLIs do report a session id mid-run; if so the
    // title fills in live. If not, `finish_oneshot` still handles it at exit.
    if agent.has_session_titles() {
        let st = state.clone();
        let ag = agent.clone();
        let live = info.id.clone();
        let tid = task.id;
        let watch_cwd = title_cwd.clone();
        tokio::spawn(async move {
            let _ =
                crate::executor::watch_title_while_running(&st, tid, ag, &live, &watch_cwd).await;
        });
    }

    // Finish the task once the interactive session ends, capturing the agent's
    // own session id (when the CLI reported one) to resolve its title.
    {
        let state = state.clone();
        let task_id = task.id;
        let session = info.id.clone();
        let agent_name = name.clone();
        let title_cwd = title_cwd.clone();
        let (detect, quiet) = (
            state.config.executor.detect_awaiting_input,
            std::time::Duration::from_millis(state.config.executor.awaiting_input_quiet_ms),
        );
        tokio::spawn(async move {
            let code = if detect {
                crate::attention::watch(&state, &session, agent, Some(task_id), quiet).await
            } else {
                state.agents.wait(&session).await
            };
            let session_id = state.agents.external_session_id(&session);
            finish_oneshot(&state, task_id, &agent_name, session_id, &title_cwd, code).await;
        });
    }

    let (session_info, frame) = state.agents.attach(&info.id)?;
    Ok(serde_json::json!({
        "task": task,
        "session": session_info,
        "data": base64::engine::general_purpose::STANDARD.encode(&frame),
    }))
}

/// Mark a one-shot task finished once its interactive session exits.
///
/// An interactive opencode session may not emit a JSON `sessionID` on its PTY,
/// so `session_id` can be `None`; the task then keeps a blank title. When the
/// agent did provide one, the title is resolved with a bounded retry and kept on
/// both success and failure.
async fn finish_oneshot(
    state: &Arc<State>,
    task_id: Uuid,
    agent_name: &str,
    session_id: Option<String>,
    cwd: &std::path::Path,
    code: Option<i32>,
) {
    let Ok(Some(mut task)) = db::get_task(&state.db, task_id).await else {
        return;
    };
    if !matches!(task.status, TaskStatus::Running | TaskStatus::AwaitingInput) {
        return;
    }
    let success = code == Some(0);

    // Persist the session even if the agent did not title it, and resolve the
    // title (with retry) when there is an id. Only overwrite the title when the
    // exit-time lookup actually found one, so a title persisted mid-run by the
    // watcher is preserved.
    task.session_id = session_id.clone();
    if let Some(sid) = session_id.as_deref() {
        if let Some(agent) = state.registry.get(agent_name) {
            if let Some(title) = crate::agents::resolve_session_title(
                agent,
                sid,
                cwd,
                crate::agents::TITLE_POLL_ATTEMPTS,
                crate::agents::TITLE_POLL_INTERVAL,
            )
            .await
            {
                task.session_title = Some(title);
            }
        }
    }

    task.status = if success {
        TaskStatus::Succeeded
    } else {
        TaskStatus::Failed
    };
    task.error = if success {
        None
    } else {
        Some(format!(
            "agent session exited with {}",
            code.map(|c| c.to_string())
                .unwrap_or_else(|| "?".to_string())
        ))
    };
    task.finished_at = Some(Utc::now());
    let _ = db::upsert_task(&state.db, &task).await;
    state
        .bus
        .publish(ServerPush::TaskUpdated(Box::new(task.summary())));
    state
        .emit_event(
            if success {
                EventKind::TaskCompleted
            } else {
                EventKind::TaskFailed
            },
            serde_json::json!({ "task_id": task.id }),
        )
        .await;
    state
        .emit_event(
            EventKind::TaskFinished,
            crate::executor::finished_payload(&task, success),
        )
        .await;
}

/// Create or update a schedule: (re)register its cron job and persist it.
async fn upsert_schedule(
    state: &Arc<State>,
    params: &serde_json::Value,
) -> anyhow::Result<favetto_core::model::Schedule> {
    use favetto_core::model::Schedule;

    let ScheduleUpsertParams {
        id,
        cron,
        task,
        input,
        enabled,
    } = parse_value(params)?;
    let id = id.unwrap_or_else(|| Uuid::new_v4().to_string());
    let input = input.unwrap_or(serde_json::Value::Null);
    let enabled = enabled.unwrap_or(true);

    let schedule = Schedule {
        id,
        cron,
        task,
        input,
        enabled,
        last_run: None,
    };

    crate::scheduler::upsert(state, &schedule).await?;
    Ok(schedule)
}

/// Add a task definition to the catalog (writes the `.md` file + updates memory).
async fn add_catalog_task(
    state: &Arc<State>,
    params: &serde_json::Value,
) -> anyhow::Result<crate::tasks::TaskDef> {
    use crate::tasks::TaskDef;

    let CatalogAddParams {
        name,
        agent,
        provider,
        model,
        cwd,
        schedule,
        needs,
        spawn,
        spawn_file,
        spawn_new_root,
        prompt,
    } = parse_value(params)?;
    crate::tasks::validate_task_path(&name)?;
    if agent.is_none() {
        anyhow::bail!("a task needs an 'agent'");
    }
    let spawn_new_root = spawn_new_root.unwrap_or(false);
    let prompt = prompt.unwrap_or_default();

    let def = TaskDef {
        name,
        agent,
        provider,
        model,
        cwd,
        schedule,
        needs,
        spawn,
        spawn_file,
        spawn_new_root,
        sign: None,
        vars: Vec::new(),
        prompt,
    };
    crate::tasks::write_task_md(&state.tasks_dir, &def)?;

    // Update the live catalog.
    {
        let mut catalog = state.catalog.write();
        catalog.retain(|d| d.name != def.name);
        catalog.push(def.clone());
        catalog.sort_by(|a, b| a.name.cmp(&b.name));
    }

    if let Err(e) = crate::workflow::regenerate(&state.catalog.read(), &state.data_dir) {
        tracing::warn!(error = %e, "failed to write workflow.dot");
    }

    // Tasks added at runtime must register their cron schedule immediately
    // (the file watcher early-returns because memory already matches disk).
    if let Err(e) = crate::scheduler::reconcile_catalog_schedules(state).await {
        tracing::warn!(error = %e, "failed to reconcile catalog schedules");
    }

    // Tell connected clients (including an open workflow view) to re-fetch.
    state.bus.publish(ServerPush::CatalogUpdated);

    Ok(def)
}

/// Replace an existing catalog task's `.md` file with `markdown` and reload.
///
/// Validates the path and the Markdown before writing so a malformed save can
/// never clobber a good task file; the raw bytes are written unchanged so the
/// user's formatting is preserved.
async fn update_catalog_task(state: &Arc<State>, params: &serde_json::Value) -> anyhow::Result<()> {
    let CatalogUpdateParams { name, markdown } = parse_value(params)?;
    crate::tasks::validate_task_path(&name)?;
    if !state.catalog.read().iter().any(|d| d.name == name) {
        anyhow::bail!("unknown task '{name}'");
    }
    crate::tasks::parse_task_md(&name, &markdown)
        .map_err(|e| anyhow::anyhow!("invalid task '{name}': {e}"))?;
    let path = state.tasks_dir.join(format!("{name}.md"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, &markdown)?;
    crate::catalog_watch::reload(state).await;
    Ok(())
}

async fn delete_schedule(state: &Arc<State>, id: &str) -> anyhow::Result<()> {
    crate::scheduler::remove(state, id).await
}

/// Add a notification hook reacting to an event kind (live, in-memory).
fn upsert_hook(state: &Arc<State>, params: &serde_json::Value) -> anyhow::Result<()> {
    let HookUpsertParams {
        event,
        channel,
        config,
    } = parse_value(params)?;
    let event = favetto_core::model::EventKind::from_name(&event)
        .ok_or_else(|| anyhow::anyhow!("missing or invalid 'event' kind"))?;
    let config = config.unwrap_or(serde_json::Value::Null);

    let hook = crate::hooks::notify_hook(event, channel, config);
    state.hook_store.write().push(hook);
    tracing::info!("added notification hook");
    Ok(())
}

/// Re-enqueue a terminal task for a fresh attempt, preserving its run history.
///
/// Rejected while a run is live: the task must be terminal and have no active
/// run row. The next claim records `attempt + 1`; earlier runs remain in the
/// history.
async fn retry_task(state: &State, id: Uuid) -> Result<favetto_core::model::Task, RpcError> {
    let task = match db::get_task(&state.db, id).await {
        Ok(Some(task)) => task,
        Ok(None) => return Err(RpcError::InvalidParams("task not found".to_string())),
        Err(e) => return Err(RpcError::Internal(e.to_string())),
    };
    if let Ok(Some(run)) = db::get_active_task_run(&state.db, id).await {
        return Err(RpcError::InvalidParams(format!(
            "task '{}' still has a live run (attempt {})",
            task.name, run.attempt
        )));
    }
    if !crate::executor::is_terminal(task.status) {
        return Err(RpcError::InvalidParams(format!(
            "task '{}' is not terminal (status '{}')",
            task.name,
            task.status.as_str()
        )));
    }
    match db::requeue_terminal_task(&state.db, id).await {
        Ok(true) => {}
        Ok(false) => {
            return Err(RpcError::InvalidParams(format!(
                "task '{}' could not be re-enqueued",
                task.name
            )))
        }
        Err(e) => return Err(RpcError::Internal(e.to_string())),
    }

    let fresh = match db::get_task(&state.db, id).await {
        Ok(Some(task)) => task,
        Ok(None) => {
            return Err(RpcError::Internal(
                "task disappeared after retry".to_string(),
            ))
        }
        Err(e) => return Err(RpcError::Internal(e.to_string())),
    };
    state
        .bus
        .publish(crate::event_bus::ServerPush::TaskUpdated(Box::new(
            fresh.summary(),
        )));
    state
        .emit_event(
            favetto_core::model::EventKind::TaskIdle,
            serde_json::json!({ "name": fresh.name, "task_id": fresh.id }),
        )
        .await;
    Ok(fresh.summary())
}

/// Mark a task cancelled, persist it, and announce the change.
async fn cancel_task(state: &State, id: Uuid) -> anyhow::Result<favetto_core::model::Task> {
    let Some(mut task) = db::get_task(&state.db, id).await? else {
        anyhow::bail!("task not found");
    };
    task.status = favetto_core::model::TaskStatus::Cancelled;
    task.finished_at = Some(chrono::Utc::now());
    db::upsert_task(&state.db, &task).await?;

    state
        .bus
        .publish(crate::event_bus::ServerPush::TaskUpdated(Box::new(
            task.summary(),
        )));

    let event = favetto_core::model::Event {
        id: 0,
        kind: favetto_core::model::EventKind::TaskCancelled,
        payload: serde_json::json!({ "task_id": task.id }),
        created_at: chrono::Utc::now(),
    };
    if let Ok(event_id) = db::insert_event(&state.db, &event).await {
        let event = favetto_core::model::Event {
            id: event_id,
            ..event
        };
        state
            .bus
            .publish(crate::event_bus::ServerPush::Event(event));
    }

    Ok(task.summary())
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
