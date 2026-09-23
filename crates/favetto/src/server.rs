//! Request dispatch and the connection lifecycle shared by both transports.
//!
//! `serve_connection` is transport-agnostic: it takes a `Stream` of incoming frames
//! and a `Sink` for outgoing frames. The Unix-socket and WebSocket transports are
//! just different ways of producing those two halves, which is why local attach is
//! merely a special case of remote attach.

use std::collections::HashSet;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use chrono::Utc;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

use favetto_core::model::{AgentCatalogEntry, AgentSessionInfo, EventKind, Task, TaskStatus};
use favetto_core::rpc::{error_code, method, push, Frame, Notification, Request, Response};
use favetto_core::wire::WireError;

use crate::agents::{AgentContext, AgentEvent, Invocation};
use crate::db;
use crate::event_bus::ServerPush;
use crate::state::State;

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
                    if push_tx
                        .send(Frame::Notification(push.into_notification()))
                        .await
                        .is_err()
                    {
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
                        if !subscribed.lock().unwrap().contains(&sid) {
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
    crate::metrics::inc_rpc();

    if req.method == method::EVENTS_SUBSCRIBE {
        let _ = out_tx
            .send(Frame::Response(Response::ok(
                id,
                serde_json::json!({ "subscribed": true }),
            )))
            .await;

        if let Some(last) = req.params.get("last_event_id").and_then(|v| v.as_i64()) {
            match db::events_after(&state.db, last, 500).await {
                Ok(events) => {
                    for ev in events {
                        if out_tx
                            .send(Frame::Notification(ev.into_notification()))
                            .await
                            .is_err()
                        {
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
                    subscribed.lock().unwrap().insert(session.id.clone());
                    Response::ok(
                        id,
                        serde_json::json!({
                            "session": session,
                            "data": base64::engine::general_purpose::STANDARD.encode(&data),
                        }),
                    )
                }
                Err(e) => Response::err(id, error_code::INTERNAL, e.to_string()),
            },
            Err(e) => Response::err(id, error_code::INVALID_PARAMS, e.to_string()),
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
                    subscribed.lock().unwrap().insert(sid.to_string());
                }
                Response::ok(id, value)
            }
            Err(e) => Response::err(id, error_code::INTERNAL, e.to_string()),
        };
        let _ = out_tx.send(Frame::Response(resp)).await;
        return;
    }

    // `agents.attach` subscribes this connection; `agents.close` unsubscribes.
    if req.method == method::AGENTS_ATTACH {
        let resp = match session_id(&req.params) {
            Ok(sid) => match state.agents.attach(&sid) {
                Ok((session, data)) => {
                    subscribed.lock().unwrap().insert(sid);
                    Response::ok(
                        id,
                        serde_json::json!({
                            "session": session,
                            "data": base64::engine::general_purpose::STANDARD.encode(&data),
                        }),
                    )
                }
                Err(e) => Response::err(id, error_code::INTERNAL, e.to_string()),
            },
            Err(e) => Response::err(id, error_code::INVALID_PARAMS, e),
        };
        let _ = out_tx.send(Frame::Response(resp)).await;
        return;
    }

    if req.method == method::AGENTS_CLOSE {
        let resp = match session_id(&req.params) {
            Ok(sid) => {
                subscribed.lock().unwrap().remove(&sid);
                match state.agents.close(&sid) {
                    Ok(()) => Response::ok(id, serde_json::json!({ "closed": sid })),
                    Err(e) => Response::err(id, error_code::INTERNAL, e.to_string()),
                }
            }
            Err(e) => Response::err(id, error_code::INVALID_PARAMS, e),
        };
        let _ = out_tx.send(Frame::Response(resp)).await;
        return;
    }

    let resp = dispatch(state, req).await;
    let _ = out_tx.send(Frame::Response(resp)).await;
}

/// Extract a required `session_id` string param.
fn session_id(params: &serde_json::Value) -> Result<String, String> {
    params
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| "missing session_id".to_string())
}

/// Execute a request and build its response.
pub async fn dispatch(state: &Arc<State>, req: Request) -> Response {
    let result: Result<serde_json::Value, (i32, String)> = match req.method.as_str() {
        method::PING => Ok(serde_json::json!({
            "pong": true,
            "cwd": std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        })),

        method::TASKS_LIST => {
            let limit = req
                .params
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(500)
                .min(2000) as i64;
            match db::list_tasks(&state.db, limit).await {
                Ok(tasks) => Ok(serde_json::json!(tasks)),
                Err(e) => Err((error_code::INTERNAL, e.to_string())),
            }
        }

        method::TASKS_GET => {
            let id = req
                .params
                .get("id")
                .and_then(|v| v.as_str())
                .and_then(|s| Uuid::parse_str(s).ok());
            match id {
                Some(id) => match db::get_task(&state.db, id).await {
                    Ok(Some(task)) => Ok(serde_json::json!(task)),
                    Ok(None) => Err((error_code::INVALID_PARAMS, "task not found".to_string())),
                    Err(e) => Err((error_code::INTERNAL, e.to_string())),
                },
                None => Err((
                    error_code::INVALID_PARAMS,
                    "missing or invalid 'id'".to_string(),
                )),
            }
        }

        method::TASKS_CANCEL => {
            let id = req
                .params
                .get("id")
                .and_then(|v| v.as_str())
                .and_then(|s| Uuid::parse_str(s).ok());
            match id {
                Some(id) => match cancel_task(state, id).await {
                    Ok(task) => Ok(serde_json::json!(task)),
                    Err(e) => Err((error_code::INTERNAL, e.to_string())),
                },
                None => Err((
                    error_code::INVALID_PARAMS,
                    "missing or invalid 'id'".to_string(),
                )),
            }
        }

        method::TASKS_START => {
            let name = match req.params.get("name").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return Response::err(req.id, error_code::INVALID_PARAMS, "missing name"),
            };
            let input = req
                .params
                .get("input")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            match crate::executor::enqueue_task(state, name, input, None).await {
                Ok(task) => Ok(serde_json::json!(task)),
                Err(e) => Err((error_code::INTERNAL, e.to_string())),
            }
        }

        method::CATALOG_LIST => {
            let catalog = state.catalog.read().unwrap().clone();
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

        method::CATALOG_GET => match req.params.get("name").and_then(|v| v.as_str()) {
            None => Err((error_code::INVALID_PARAMS, "missing 'name'".to_string())),
            Some(name) => {
                if let Err(e) = crate::tasks::validate_task_path(name) {
                    Err((error_code::INVALID_PARAMS, e.to_string()))
                } else {
                    let path = state.tasks_dir.join(format!("{name}.md"));
                    let markdown = std::fs::read_to_string(&path).ok().or_else(|| {
                        state
                            .catalog
                            .read()
                            .unwrap()
                            .iter()
                            .find(|d| d.name == name)
                            .map(crate::tasks::to_markdown)
                    });
                    match markdown {
                        Some(markdown) => Ok(serde_json::json!({ "markdown": markdown })),
                        None => Err((error_code::INVALID_PARAMS, format!("unknown task '{name}'"))),
                    }
                }
            }
        },

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
            Err(e) => Err((error_code::INTERNAL, e.to_string())),
        },

        method::CATALOG_UPDATE => match update_catalog_task(state, &req.params).await {
            Ok(()) => Ok(serde_json::json!({ "updated": true })),
            Err(e) => Err((error_code::INVALID_PARAMS, e.to_string())),
        },

        method::WORKFLOW_GET => {
            let catalog = state.catalog.read().unwrap().clone();
            Ok(serde_json::json!({
                "dot": crate::workflow::build_dot(&catalog),
                "path": crate::workflow::dot_path(&state.data_dir).to_string_lossy(),
                "graph": crate::workflow::build_graph(&catalog),
            }))
        }

        method::EVENTS_TAIL => {
            let limit = req
                .params
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(50)
                .min(1000) as i64;
            match db::tail_events(&state.db, limit).await {
                Ok(events) => Ok(serde_json::json!(events)),
                Err(e) => Err((error_code::INTERNAL, e.to_string())),
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
            let agent = req.params.get("agent").and_then(|v| v.as_str());
            match list_providers(state, agent).await {
                Ok(providers) => Ok(serde_json::json!({ "providers": providers })),
                Err(e) => Err((error_code::INTERNAL, e.to_string())),
            }
        }

        method::AGENTS_INPUT => {
            let sid = match session_id(&req.params) {
                Ok(s) => s,
                Err(e) => return Response::err(req.id, error_code::INVALID_PARAMS, e),
            };
            let data = req
                .params
                .get("data")
                .and_then(|v| v.as_str())
                .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
                .unwrap_or_default();
            match state.agents.input(&sid, &data) {
                Ok(()) => Ok(serde_json::json!({ "bytes": data.len() })),
                Err(e) => Err((error_code::INTERNAL, e.to_string())),
            }
        }

        method::AGENTS_RESIZE => {
            let sid = match session_id(&req.params) {
                Ok(s) => s,
                Err(e) => return Response::err(req.id, error_code::INVALID_PARAMS, e),
            };
            let rows = req
                .params
                .get("rows")
                .and_then(|v| v.as_u64())
                .unwrap_or(24) as u16;
            let cols = req
                .params
                .get("cols")
                .and_then(|v| v.as_u64())
                .unwrap_or(80) as u16;
            match state.agents.resize(&sid, rows, cols) {
                Ok(()) => Ok(serde_json::json!({ "rows": rows, "cols": cols })),
                Err(e) => Err((error_code::INTERNAL, e.to_string())),
            }
        }

        method::SCHEDULES_LIST => match db::list_schedules(&state.db).await {
            Ok(schedules) => Ok(serde_json::json!(schedules)),
            Err(e) => Err((error_code::INTERNAL, e.to_string())),
        },

        method::SCHEDULES_UPSERT => match upsert_schedule(state, &req.params).await {
            Ok(schedule) => Ok(serde_json::json!(schedule)),
            Err(e) => Err((error_code::INTERNAL, e.to_string())),
        },

        method::SCHEDULES_DELETE => {
            let id = match req.params.get("id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return Response::err(req.id, error_code::INVALID_PARAMS, "missing id"),
            };
            match delete_schedule(state, &id).await {
                Ok(()) => Ok(serde_json::json!({ "deleted": id })),
                Err(e) => Err((error_code::INTERNAL, e.to_string())),
            }
        }

        method::NOTIFICATIONS_LIST => {
            let limit = req
                .params
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(50)
                .min(1000) as i64;
            match db::list_notifications(&state.db, limit).await {
                Ok(notifications) => Ok(serde_json::json!(notifications)),
                Err(e) => Err((error_code::INTERNAL, e.to_string())),
            }
        }

        method::NOTIFICATIONS_TEST => {
            let channel = match req.params.get("channel").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => {
                    return Response::err(req.id, error_code::INVALID_PARAMS, "missing channel")
                }
            };
            let config = req
                .params
                .get("config")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let subject = req
                .params
                .get("subject")
                .and_then(|v| v.as_str())
                .unwrap_or("test")
                .to_string();
            let body = req
                .params
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or("test notification")
                .to_string();
            crate::notify::send(state, &channel, &config, &subject, &body).await;
            Ok(serde_json::json!({ "sent": true }))
        }

        method::HOOKS_UPSERT => match upsert_hook(state, &req.params) {
            Ok(()) => Ok(serde_json::json!({ "added": true })),
            Err(e) => Err((error_code::INVALID_PARAMS, e.to_string())),
        },

        _ => Err((
            error_code::METHOD_NOT_FOUND,
            format!("unknown method: {}", req.method),
        )),
    };

    match result {
        Ok(value) => Response::ok(req.id, value),
        Err((code, message)) => Response::err(req.id, code, message),
    }
}

/// Resolve the requested (or default) agent and spawn a PTY session.
///
/// Task runs are headless (their PTY carries machine output such as JSON), so
/// attaching never shows that PTY: when a session id is known — from the task row
/// or captured from a still-running run — the agent's `resume_args` open a real
/// interactive TUI. A live interactive session is attached directly.
async fn start_agent(
    state: &Arc<State>,
    params: &serde_json::Value,
) -> anyhow::Result<AgentSessionInfo> {
    let requested = params
        .get("agent")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let task_id = params
        .get("task_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let prompt = params
        .get("prompt")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let session_id = params
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let rows = params.get("rows").and_then(|v| v.as_u64()).unwrap_or(24) as u16;
    let cols = params.get("cols").and_then(|v| v.as_u64()).unwrap_or(80) as u16;

    let force_new = params.get("new").and_then(|v| v.as_bool()).unwrap_or(false);

    // Resolve the agent in a scoped block so the config read guard is dropped
    // before the `await` below (a std `RwLockReadGuard` is not `Send`).
    let name = {
        let cfg = state.config.read().unwrap();
        requested
            .or_else(|| cfg.agent.default.clone())
            .ok_or_else(|| {
                anyhow::anyhow!("no agent given and no default configured ([agent].default)")
            })?
    };
    let agent = state.registry.get_checked(&name)?;

    // A catalog task's prompt is a template over its collected `input`. Render it
    // here so a freshly seeded interactive session sees the same text a headless
    // run would, instead of raw `{{ input.* }}` placeholders. Pick up the task's
    // `provider`/`model` too, so the panel session matches the run instead of
    // silently falling back to the agent default. The request wins; the task
    // definition fills in whatever it didn't send.
    let requested_provider = params
        .get("provider")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let requested_model = params
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::to_string);
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

    let ctx = AgentContext {
        cwd: params
            .get("cwd")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from),
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

        // Otherwise resume the agent's own session, never the headless run's PTY
        // (whose screen is machine output). The id comes from the request, the
        // task row, or the running run once its output has been parsed.
        if agent.capabilities().resume {
            let sid = match session_id.clone() {
                Some(sid) => Some(sid),
                None => match persisted_session_id(state, task_id.as_deref()).await {
                    Some(sid) => Some(sid),
                    None => wait_for_run_session_id(state, task_id.as_deref()).await,
                },
            };
            if let Some(sid) = sid {
                return state
                    .agents
                    .start(&name, agent, task_id, Invocation::Resume(&sid), ctx);
            }
        }

        // Legacy: replay a finished non-headless session's screen.
        if let Some(existing) = &live {
            if !existing.headless {
                return Ok(existing.clone());
            }
        }

        // A still-running headless run owns this task. Its PTY carries machine
        // output, so it cannot be shown in the panel, and seeding a fresh
        // interactive session here would start a duplicate run that re-submits
        // the prompt. If its agent session id never surfaced (the resume/wait
        // above), report that instead of racing a second run.
        if let Some(existing) = task_id
            .as_deref()
            .and_then(|tid| state.agents.find_latest_by_task(tid))
        {
            if existing.running && existing.headless {
                return Err(anyhow::anyhow!(
                    "task is already running headless; its agent session is not ready to attach yet"
                ));
            }
        }
    }

    state.agents.start(
        &name,
        agent,
        task_id,
        Invocation::Interactive {
            prompt: prompt.as_deref(),
            provider: provider.as_deref(),
            model: model.as_deref(),
        },
        ctx,
    )
}

/// Whether the daemon should reattach to an existing session rather than launch
/// a fresh one: any live interactive TUI, or a live headless run that is blocked
/// waiting for the user (so keystrokes can answer the prompt).
fn should_attach_to_live(info: &AgentSessionInfo) -> bool {
    info.running && (!info.headless || info.awaiting_input.is_some())
}

/// Wait briefly for a still-running headless run to publish its agent session id
/// (parsed from its JSON output). Returns `None` if there is no running run.
async fn wait_for_run_session_id(state: &Arc<State>, task_id: Option<&str>) -> Option<String> {
    let task_id = task_id?;
    for _ in 0..50 {
        let session = state.agents.find_latest_by_task(task_id)?;
        if !session.running {
            return None;
        }
        if let Some(sid) = state.agents.external_session_id(&session.id) {
            return Some(sid);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    None
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
        .unwrap()
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
    let requested = params
        .get("agent")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let provider = params
        .get("provider")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let model = params
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let cwd = params
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let rows = params.get("rows").and_then(|v| v.as_u64()).unwrap_or(24) as u16;
    let cols = params.get("cols").and_then(|v| v.as_u64()).unwrap_or(80) as u16;

    let name = {
        let cfg = state.config.read().unwrap();
        requested
            .or_else(|| cfg.agent.default.clone())
            .ok_or_else(|| {
                anyhow::anyhow!("no agent given and no default configured ([agent].default)")
            })?
    };
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
        session_id: None,
        session_title: None,
        parent_id: None,
        root_id: None,
    };
    db::insert_task(&state.db, &task).await?;
    crate::metrics::inc_tasks();
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

    let ctx = AgentContext {
        cwd: cwd_path,
        provider: provider.clone(),
        model: model.clone(),
        rows,
        cols,
        ..Default::default()
    };
    let info = state.agents.start(
        &name,
        agent.clone(),
        Some(task.id.to_string()),
        Invocation::Interactive {
            prompt: None,
            provider: provider.as_deref(),
            model: model.as_deref(),
        },
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
        let (detect, quiet) = {
            let cfg = state.config.read().unwrap();
            (
                cfg.executor.detect_awaiting_input,
                std::time::Duration::from_millis(cfg.executor.awaiting_input_quiet_ms),
            )
        };
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
            serde_json::json!({ "name": task.name, "task_id": task.id, "success": success }),
        )
        .await;
}

/// Create or update a schedule: (re)register its cron job and persist it.
async fn upsert_schedule(
    state: &Arc<State>,
    params: &serde_json::Value,
) -> anyhow::Result<favetto_core::model::Schedule> {
    use favetto_core::model::Schedule;

    let id = params
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let cron = params
        .get("cron")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing cron"))?
        .to_string();
    let task = params
        .get("task")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing task"))?
        .to_string();
    let input = params
        .get("input")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let enabled = params
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

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

    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'name'"))?
        .to_string();
    crate::tasks::validate_task_path(&name)?;
    let agent = params
        .get("agent")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let provider = params
        .get("provider")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let model = params
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let cwd = params
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    if agent.is_none() {
        anyhow::bail!("a task needs an 'agent'");
    }
    let schedule = params
        .get("schedule")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let needs = params
        .get("needs")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let spawn = params
        .get("spawn")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let spawn_file = params
        .get("spawn_file")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let prompt = params
        .get("prompt")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

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
        sign: None,
        vars: Vec::new(),
        prompt,
    };
    crate::tasks::write_task_md(&state.tasks_dir, &def)?;

    // Update the live catalog.
    {
        let mut catalog = state.catalog.write().unwrap();
        catalog.retain(|d| d.name != def.name);
        catalog.push(def.clone());
        catalog.sort_by(|a, b| a.name.cmp(&b.name));
    }

    if let Err(e) = crate::workflow::regenerate(&state.catalog.read().unwrap(), &state.data_dir) {
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
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'name'"))?
        .to_string();
    crate::tasks::validate_task_path(&name)?;
    let markdown = params
        .get("markdown")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'markdown'"))?;
    if !state.catalog.read().unwrap().iter().any(|d| d.name == name) {
        anyhow::bail!("unknown task '{name}'");
    }
    crate::tasks::parse_task_md(&name, markdown)
        .map_err(|e| anyhow::anyhow!("invalid task '{name}': {e}"))?;
    let path = state.tasks_dir.join(format!("{name}.md"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, markdown)?;
    crate::catalog_watch::reload(state).await;
    Ok(())
}

async fn delete_schedule(state: &Arc<State>, id: &str) -> anyhow::Result<()> {
    crate::scheduler::remove(state, id).await
}

/// Add a notification hook reacting to an event kind (live, in-memory).
fn upsert_hook(state: &Arc<State>, params: &serde_json::Value) -> anyhow::Result<()> {
    let event = params
        .get("event")
        .and_then(|v| v.as_str())
        .and_then(favetto_core::model::EventKind::from_name)
        .ok_or_else(|| anyhow::anyhow!("missing or invalid 'event' kind"))?;
    let channel = params
        .get("channel")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'channel'"))?
        .to_string();
    let config = params
        .get("config")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    let hook = crate::hooks::notify_hook(event, channel, config);
    state.hook_store.write().unwrap().push(hook);
    tracing::info!("added notification hook");
    Ok(())
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

/// Build a log-line push (used by later milestones; wired up for completeness).
#[allow(dead_code)]
pub fn log_line(level: &str, message: impl Into<String>) -> Notification {
    Notification {
        method: push::LOG_LINE.to_string(),
        params: serde_json::json!({ "level": level, "message": message.into() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A unique scratch directory for server tests.
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "favetto-server-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A `State` backed by a scratch SQLite database and the given tasks dir.
    async fn test_state(dir: &Path, tasks_dir: &Path) -> Arc<State> {
        let pool = crate::db::open(&dir.join("test.db")).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let catalog = Arc::new(std::sync::RwLock::new(
            crate::tasks::load_catalog(tasks_dir).unwrap(),
        ));
        let scheduler = tokio_cron_scheduler::JobScheduler::new().await.unwrap();
        Arc::new(State::new(
            pool,
            crate::event_bus::EventBus::new(64),
            favetto_core::auth::Token::generate(),
            crate::webhooks::WebhookSecrets::from_config(&crate::config::FavettoConfig::default()),
            crate::agents::AgentManager::new(),
            crate::agents::AgentRegistry::default(),
            Arc::new(std::sync::RwLock::new(
                crate::config::FavettoConfig::default(),
            )),
            dir.to_path_buf(),
            tasks_dir.to_path_buf(),
            catalog,
            scheduler,
            Arc::new(std::sync::RwLock::new(Vec::new())),
        ))
    }

    #[tokio::test]
    async fn workflow_get_returns_dot_and_path() {
        let dir = temp_dir("workflow-get");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join("a.md"),
            "agent = \"x\"\nspawn = \"b\"\n---\nprompt a\n",
        )
        .unwrap();
        let state = test_state(&dir, &tasks_dir).await;

        let req = Request {
            id: 1,
            method: method::WORKFLOW_GET.to_string(),
            params: serde_json::json!({}),
        };
        let resp = dispatch(&state, req).await;
        let result = resp.result.expect("workflow.get result");
        let dot = result.get("dot").and_then(|d| d.as_str()).unwrap();
        assert!(dot.contains("\"a\" -> \"b\" [label=\"spawn\"];"), "{dot}");
        let path = result.get("path").and_then(|p| p.as_str()).unwrap();
        assert!(path.ends_with("workflow.dot"), "{path}");

        let graph = result.get("graph").expect("graph field");
        let nodes = graph
            .get("nodes")
            .and_then(|n| n.as_array())
            .expect("graph nodes");
        assert!(
            nodes
                .iter()
                .any(|n| n["name"] == "a" && n["scheduled"] == false && n["external"] == false),
            "{graph}"
        );
        assert!(
            nodes
                .iter()
                .any(|n| n["name"] == "b" && n["external"] == true),
            "{graph}"
        );
        let edges = graph
            .get("edges")
            .and_then(|e| e.as_array())
            .expect("graph edges");
        assert_eq!(
            edges[0],
            serde_json::json!({ "from": "a", "to": "b", "kind": "spawn" })
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn add_catalog_task_writes_dot_and_pushes_catalog_updated() {
        let dir = temp_dir("add");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let state = test_state(&dir, &tasks_dir).await;
        let mut rx = state.bus.subscribe();

        let params = serde_json::json!({
            "name": "newtask",
            "agent": "x",
            "prompt": "hello",
        });
        let def = add_catalog_task(&state, &params).await.unwrap();
        assert_eq!(def.name, "newtask");

        let dot = std::fs::read_to_string(dir.join("workflow.dot")).unwrap();
        assert!(dot.contains("\"newtask\" [label=\"newtask\"];"), "{dot}");
        assert!(matches!(rx.try_recv(), Ok(ServerPush::CatalogUpdated)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn add_catalog_task_dot_reflects_spawn() {
        let dir = temp_dir("add-spawn");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(tasks_dir.join("child.md"), "agent = \"x\"\n---\nbody\n").unwrap();
        let state = test_state(&dir, &tasks_dir).await;

        let params = serde_json::json!({
            "name": "parent",
            "agent": "x",
            "spawn": "child",
        });
        add_catalog_task(&state, &params).await.unwrap();

        let dot = std::fs::read_to_string(dir.join("workflow.dot")).unwrap();
        assert!(
            dot.contains("\"parent\" -> \"child\" [label=\"spawn\"];"),
            "{dot}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn add_catalog_task_with_schedule_registers_cron() {
        let dir = temp_dir("add-schedule");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let state = test_state(&dir, &tasks_dir).await;

        let params = serde_json::json!({
            "name": "scheduled",
            "agent": "x",
            "prompt": "hello",
            "schedule": "0 0 8 * * *",
        });
        add_catalog_task(&state, &params).await.unwrap();

        let scheduled = crate::db::list_schedules(&state.db).await.unwrap();
        let entry = scheduled
            .iter()
            .find(|s| s.id == "catalog:scheduled")
            .expect("catalog schedule registered on add");
        assert_eq!(entry.cron, "0 0 8 * * *");
        assert_eq!(entry.task, "scheduled");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn update_catalog_task_rewrites_file_and_publishes() {
        let dir = temp_dir("update");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(tasks_dir.join("a.md"), "agent = \"x\"\n---\noriginal\n").unwrap();
        let state = test_state(&dir, &tasks_dir).await;
        let mut rx = state.bus.subscribe();

        let updated = "agent = \"x\"\n---\nupdated body\n";
        let params = serde_json::json!({ "name": "a", "markdown": updated });
        update_catalog_task(&state, &params).await.unwrap();

        assert_eq!(
            std::fs::read_to_string(tasks_dir.join("a.md")).unwrap(),
            updated
        );
        let prompt = state
            .catalog
            .read()
            .unwrap()
            .iter()
            .find(|d| d.name == "a")
            .map(|d| d.prompt.clone())
            .unwrap();
        assert_eq!(prompt, "updated body");
        assert!(matches!(rx.try_recv(), Ok(ServerPush::CatalogUpdated)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn update_catalog_task_nested_path() {
        let dir = temp_dir("update-nested");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(tasks_dir.join("pipelines")).unwrap();
        std::fs::write(
            tasks_dir.join("pipelines/plan.md"),
            "agent = \"x\"\n---\none\n",
        )
        .unwrap();
        let state = test_state(&dir, &tasks_dir).await;

        let updated = "agent = \"x\"\n---\ntwo\n";
        let params = serde_json::json!({ "name": "pipelines/plan", "markdown": updated });
        update_catalog_task(&state, &params).await.unwrap();

        assert_eq!(
            std::fs::read_to_string(tasks_dir.join("pipelines/plan.md")).unwrap(),
            updated
        );
        assert!(state
            .catalog
            .read()
            .unwrap()
            .iter()
            .any(|d| d.name == "pipelines/plan" && d.prompt == "two"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn update_catalog_task_rejects_unknown_name() {
        let dir = temp_dir("update-unknown");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let state = test_state(&dir, &tasks_dir).await;

        let params = serde_json::json!({
            "name": "ghost",
            "markdown": "agent = \"x\"\n---\nhi\n",
        });
        let err = update_catalog_task(&state, &params).await.unwrap_err();
        assert!(err.to_string().contains("unknown task"), "{err}");
        assert!(!tasks_dir.join("ghost.md").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn update_catalog_task_rejects_invalid_markdown() {
        let dir = temp_dir("update-invalid");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let original = "agent = \"x\"\n---\noriginal\n";
        std::fs::write(tasks_dir.join("a.md"), original).unwrap();
        let state = test_state(&dir, &tasks_dir).await;

        let params = serde_json::json!({ "name": "a", "markdown": "agent = \n---\nbroken\n" });
        let err = update_catalog_task(&state, &params).await.unwrap_err();
        assert!(err.to_string().contains("invalid task"), "{err}");
        assert_eq!(
            std::fs::read_to_string(tasks_dir.join("a.md")).unwrap(),
            original
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn update_catalog_task_rejects_traversal() {
        let dir = temp_dir("update-traversal");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let state = test_state(&dir, &tasks_dir).await;

        let params = serde_json::json!({
            "name": "../escape",
            "markdown": "agent = \"x\"\n---\nhi\n",
        });
        let err = update_catalog_task(&state, &params).await.unwrap_err();
        assert!(err.to_string().contains("invalid task path"), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dispatch_catalog_update_returns_updated() {
        let dir = temp_dir("update-dispatch");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(tasks_dir.join("a.md"), "agent = \"x\"\n---\noriginal\n").unwrap();
        let state = test_state(&dir, &tasks_dir).await;

        let req = Request {
            id: 1,
            method: method::CATALOG_UPDATE.to_string(),
            params: serde_json::json!({
                "name": "a",
                "markdown": "agent = \"x\"\n---\nnew\n",
            }),
        };
        let resp = dispatch(&state, req).await;
        assert!(resp.error.is_none(), "{:?}", resp.error);
        assert_eq!(
            resp.result.and_then(|v| v.get("updated").cloned()),
            Some(serde_json::json!(true))
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A fake `opencode session list --format json` executable that ignores its
    /// arguments and prints `json`.
    #[cfg(unix)]
    fn session_list_script(dir: &Path, json: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("opencode-session-list.sh");
        std::fs::write(&path, format!("#!/bin/sh\nprintf '%s' '{json}'\n")).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    /// A fake opencode that appends its argv to `capture` and exits 0.
    #[cfg(unix)]
    fn argv_capture_script(dir: &Path, capture: &Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-opencode.sh");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\n",
                capture.display()
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    /// A `State` whose registry carries the given `[agents.*]` entries and whose
    /// `[agent].default` is `default`.
    #[cfg(unix)]
    async fn agent_state(
        dir: &Path,
        tasks_dir: &Path,
        default: &str,
        agents: Vec<(&str, crate::config::AgentConfig)>,
    ) -> Arc<State> {
        let pool = crate::db::open(&dir.join("test.db")).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let mut cfg = crate::config::FavettoConfig::default();
        cfg.agent.default = Some(default.to_string());
        for (name, agent) in agents {
            cfg.agents.insert(name.to_string(), agent);
        }
        let registry = crate::agents::AgentRegistry::from_config_with(&cfg, &|_| true).unwrap();
        let catalog = Arc::new(std::sync::RwLock::new(
            crate::tasks::load_catalog(tasks_dir).unwrap(),
        ));
        let scheduler = tokio_cron_scheduler::JobScheduler::new().await.unwrap();
        Arc::new(State::new(
            pool,
            crate::event_bus::EventBus::new(64),
            favetto_core::auth::Token::generate(),
            crate::webhooks::WebhookSecrets::from_config(&cfg),
            crate::agents::AgentManager::new(),
            registry,
            Arc::new(std::sync::RwLock::new(cfg)),
            dir.to_path_buf(),
            tasks_dir.to_path_buf(),
            catalog,
            scheduler,
            Arc::new(std::sync::RwLock::new(Vec::new())),
        ))
    }

    /// A `State` whose `opencode` command is `command` (a title-lookup fixture).
    #[cfg(unix)]
    async fn title_state(dir: &Path, tasks_dir: &Path, command: &Path) -> Arc<State> {
        agent_state(
            dir,
            tasks_dir,
            "opencode",
            vec![(
                "opencode",
                crate::config::AgentConfig {
                    command: command.to_string_lossy().into_owned(),
                    ..Default::default()
                },
            )],
        )
        .await
    }

    fn oneshot_task() -> Task {
        Task {
            id: Uuid::new_v4(),
            name: "one-shot".to_string(),
            status: TaskStatus::Running,
            input: serde_json::json!({ "oneshot": true }),
            output: None,
            dedupe_key: None,
            created_at: Utc::now(),
            started_at: Some(Utc::now()),
            finished_at: None,
            error: None,
            session_id: None,
            session_title: None,
            parent_id: None,
            root_id: None,
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn finish_oneshot_records_session_id_and_title() {
        let dir = temp_dir("oneshot-title");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let script = session_list_script(&dir, r#"[{"id":"ses_1","title":"Fixture title"}]"#);
        let state = title_state(&dir, &tasks_dir, &script).await;

        let task = oneshot_task();
        db::insert_task(&state.db, &task).await.unwrap();

        finish_oneshot(
            &state,
            task.id,
            "opencode",
            Some("ses_1".to_string()),
            &dir,
            Some(0),
        )
        .await;

        let stored = db::get_task(&state.db, task.id).await.unwrap().unwrap();
        assert_eq!(stored.status, TaskStatus::Succeeded);
        assert_eq!(stored.session_id.as_deref(), Some("ses_1"));
        assert_eq!(stored.session_title.as_deref(), Some("Fixture title"));
        assert!(stored.error.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn finish_oneshot_failure_keeps_session_info() {
        let dir = temp_dir("oneshot-title-fail");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let script = session_list_script(&dir, r#"[{"id":"ses_1","title":"Fixture title"}]"#);
        let state = title_state(&dir, &tasks_dir, &script).await;

        let task = oneshot_task();
        db::insert_task(&state.db, &task).await.unwrap();

        finish_oneshot(
            &state,
            task.id,
            "opencode",
            Some("ses_1".to_string()),
            &dir,
            Some(1),
        )
        .await;

        let stored = db::get_task(&state.db, task.id).await.unwrap().unwrap();
        assert_eq!(stored.status, TaskStatus::Failed);
        assert_eq!(stored.session_id.as_deref(), Some("ses_1"));
        assert_eq!(stored.session_title.as_deref(), Some("Fixture title"));
        assert!(stored.error.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn finish_oneshot_without_session_id_is_blank() {
        let dir = temp_dir("oneshot-no-session");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let script = session_list_script(&dir, r#"[{"id":"ses_1","title":"Fixture title"}]"#);
        let state = title_state(&dir, &tasks_dir, &script).await;

        let task = oneshot_task();
        db::insert_task(&state.db, &task).await.unwrap();

        finish_oneshot(&state, task.id, "opencode", None, &dir, Some(0)).await;

        let stored = db::get_task(&state.db, task.id).await.unwrap().unwrap();
        assert_eq!(stored.status, TaskStatus::Succeeded);
        assert!(stored.session_id.is_none());
        assert!(stored.session_title.is_none());
        assert!(stored.error.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A title persisted mid-run by the watcher must survive `finish_oneshot`
    /// when the exit-time lookup finds nothing.
    #[cfg(unix)]
    #[tokio::test]
    async fn finish_oneshot_preserves_mid_run_title() {
        let dir = temp_dir("oneshot-preserve");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        // The exit-time lookup finds no sessions, so it yields no title.
        let script = session_list_script(&dir, "[]");
        let state = title_state(&dir, &tasks_dir, &script).await;

        let mut task = oneshot_task();
        task.session_id = Some("ses_1".to_string());
        task.session_title = Some("Persisted mid-run".to_string());
        db::insert_task(&state.db, &task).await.unwrap();

        finish_oneshot(
            &state,
            task.id,
            "opencode",
            Some("ses_1".to_string()),
            &dir,
            Some(0),
        )
        .await;

        let stored = db::get_task(&state.db, task.id).await.unwrap().unwrap();
        assert_eq!(stored.status, TaskStatus::Succeeded);
        assert_eq!(stored.session_id.as_deref(), Some("ses_1"));
        assert_eq!(stored.session_title.as_deref(), Some("Persisted mid-run"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn live_session(running: bool, headless: bool, awaiting: bool) -> AgentSessionInfo {
        AgentSessionInfo {
            id: "s1".to_string(),
            agent: "opencode".to_string(),
            task_id: None,
            running,
            headless,
            session_id: None,
            awaiting_input: awaiting.then(|| favetto_core::model::AwaitingInputReason {
                kind: favetto_core::model::AwaitingInputKind::Permission,
                message: "Allow?".to_string(),
            }),
        }
    }

    #[test]
    fn should_attach_to_live_allows_awaiting_headless_session() {
        // Live interactive TUI: attach.
        assert!(should_attach_to_live(&live_session(true, false, false)));
        // Running headless without a prompt: don't attach (resume instead).
        assert!(!should_attach_to_live(&live_session(true, true, false)));
        // Headless but blocked on the user: attach so keystrokes reach it.
        assert!(should_attach_to_live(&live_session(true, true, true)));
        // Exited sessions are never attached.
        assert!(!should_attach_to_live(&live_session(false, true, true)));
        assert!(!should_attach_to_live(&live_session(false, false, false)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn finish_oneshot_accepts_awaiting_input() {
        let dir = temp_dir("oneshot-awaiting");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let script = session_list_script(&dir, r#"[{"id":"ses_1","title":"Fixture title"}]"#);
        let state = title_state(&dir, &tasks_dir, &script).await;

        let mut task = oneshot_task();
        task.status = TaskStatus::AwaitingInput;
        db::insert_task(&state.db, &task).await.unwrap();

        finish_oneshot(
            &state,
            task.id,
            "opencode",
            Some("ses_1".to_string()),
            &dir,
            Some(0),
        )
        .await;

        // A task paused on input is still finished once the session exits.
        let stored = db::get_task(&state.db, task.id).await.unwrap().unwrap();
        assert_eq!(stored.status, TaskStatus::Succeeded);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn agents_start_renders_catalog_prompt_with_task_input() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("start-render");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join("issue.md"),
            "agent = \"opencode\"\n\
             [[vars]]\nname = \"repo\"\nprompt = \"Repo\"\nrequired = true\n\
             ---\nTarget repository: `{{ input.repo }}`\n",
        )
        .unwrap();

        // A fake opencode that records the argv it was launched with.
        let capture = dir.join("captured-args.txt");
        let script = dir.join("fake-opencode.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\n",
                capture.display()
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let state = title_state(&dir, &tasks_dir, &script).await;

        let task = Task {
            id: Uuid::new_v4(),
            name: "issue".to_string(),
            status: TaskStatus::Failed,
            input: serde_json::json!({ "repo": "acme/widgets" }),
            output: None,
            dedupe_key: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            error: None,
            session_id: None,
            session_title: None,
            parent_id: None,
            root_id: None,
        };
        db::insert_task(&state.db, &task).await.unwrap();

        // The client sends the raw catalog prompt; the server must render it
        // against the task's collected input before seeding the session.
        start_agent(
            &state,
            &serde_json::json!({
                "task_id": task.id.to_string(),
                "prompt": "Target repository: `{{ input.repo }}`",
                "rows": 40,
                "cols": 120,
            }),
        )
        .await
        .expect("start_agent");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let captured = loop {
            let text = std::fs::read_to_string(&capture).unwrap_or_default();
            if !text.is_empty() || std::time::Instant::now() >= deadline {
                break text;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        assert!(
            captured.contains("acme/widgets"),
            "input.repo was not rendered into the seeded prompt: {captured}"
        );
        assert!(
            !captured.contains("{{ input."),
            "a raw placeholder survived into the seeded prompt: {captured}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The panel session must inherit the catalog task's `provider`/`model`, so
    /// an interactive reattach uses the same route as the headless run.
    #[cfg(unix)]
    #[tokio::test]
    async fn agents_start_uses_catalog_provider_and_model() {
        let dir = temp_dir("start-model");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join("issue.md"),
            "agent = \"opencode\"\nprovider = \"acme\"\nmodel = \"big\"\n---\nHello\n",
        )
        .unwrap();

        let capture = dir.join("captured-args.txt");
        let script = argv_capture_script(&dir, &capture);
        let state = title_state(&dir, &tasks_dir, &script).await;

        let task = Task {
            id: Uuid::new_v4(),
            name: "issue".to_string(),
            status: TaskStatus::Failed,
            input: serde_json::json!({}),
            output: None,
            dedupe_key: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            error: None,
            session_id: None,
            session_title: None,
            parent_id: None,
            root_id: None,
        };
        db::insert_task(&state.db, &task).await.unwrap();

        start_agent(
            &state,
            &serde_json::json!({ "task_id": task.id.to_string() }),
        )
        .await
        .expect("start_agent");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let captured = loop {
            let text = std::fs::read_to_string(&capture).unwrap_or_default();
            if !text.is_empty() || std::time::Instant::now() >= deadline {
                break text;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        assert!(
            captured.contains("--model acme/big"),
            "the task's provider/model did not reach the seeded session: {captured}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End-to-end regression for issue #69: starting a catalog task through
    /// `tasks.start` alone runs it headlessly and submits the rendered prompt —
    /// no `agents.start` / Agent-panel attach is needed.
    #[cfg(unix)]
    #[tokio::test]
    async fn tasks_start_submits_rendered_prompt_without_attach() {
        let dir = temp_dir("tasks-start-no-attach");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join("issue.md"),
            "agent = \"opencode\"\n\
             [[vars]]\nname = \"repo\"\nprompt = \"Repo\"\nrequired = true\n\
             ---\nTarget repository: `{{ input.repo }}`\n",
        )
        .unwrap();

        let capture = dir.join("captured-args.txt");
        let script = argv_capture_script(&dir, &capture);
        let state = title_state(&dir, &tasks_dir, &script).await;

        // The same RPC the TUI sends: enqueue only, never `agents.start`.
        let resp = dispatch(
            &state,
            Request {
                id: 1,
                method: method::TASKS_START.to_string(),
                params: serde_json::json!({
                    "name": "issue",
                    "input": { "repo": "acme/widgets" },
                }),
            },
        )
        .await;
        assert!(resp.error.is_none(), "{:?}", resp.error);

        // The background dispatcher claims and runs the queued task.
        let executor = crate::executor::spawn(state.clone());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let captured = loop {
            let text = std::fs::read_to_string(&capture).unwrap_or_default();
            if !text.is_empty() || std::time::Instant::now() >= deadline {
                break text;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        executor.abort();

        assert!(
            captured.contains("acme/widgets"),
            "the rendered prompt was not submitted by the headless run: {captured}"
        );
        assert!(
            !captured.contains("{{ input."),
            "a raw placeholder survived into the headless prompt: {captured}"
        );
        assert!(
            state.agents.sessions().iter().all(|s| s.headless),
            "an interactive session was launched: {:?}",
            state.agents.sessions()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Opening the panel on a live unattended run must not seed a duplicate
    /// session (and re-submit the prompt) for the same task.
    #[cfg(unix)]
    #[tokio::test]
    async fn agents_start_does_not_duplicate_a_live_headless_run() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("start-no-dup");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(tasks_dir.join("issue.md"), "agent = \"plain\"\n---\nbody\n").unwrap();

        // A non-resuming agent whose runs stay alive: a live headless session
        // with no captured agent session id to resume.
        let script = dir.join("fake-plain.sh");
        std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let state = agent_state(
            &dir,
            &tasks_dir,
            "plain",
            vec![(
                "plain",
                crate::config::AgentConfig {
                    command: script.to_string_lossy().into_owned(),
                    headless_args: Some(vec!["run".to_string(), "{prompt}".to_string()]),
                    ..Default::default()
                },
            )],
        )
        .await;

        let task = Task {
            id: Uuid::new_v4(),
            name: "issue".to_string(),
            status: TaskStatus::Running,
            input: serde_json::json!({}),
            output: None,
            dedupe_key: None,
            created_at: Utc::now(),
            started_at: Some(Utc::now()),
            finished_at: None,
            error: None,
            session_id: None,
            session_title: None,
            parent_id: None,
            root_id: None,
        };
        db::insert_task(&state.db, &task).await.unwrap();

        let agent = state.registry.get_checked("plain").unwrap();
        let info = state
            .agents
            .start(
                "plain",
                agent,
                Some(task.id.to_string()),
                Invocation::Headless {
                    prompt: "body",
                    provider: None,
                    model: None,
                },
                AgentContext {
                    rows: 24,
                    cols: 80,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(info.headless && info.running);

        let err = start_agent(
            &state,
            &serde_json::json!({ "task_id": task.id.to_string(), "agent": "plain" }),
        )
        .await
        .expect_err("a live headless run must not be duplicated");
        assert!(
            err.to_string().contains("already running headless"),
            "{err}"
        );

        let sessions = state.agents.sessions();
        assert_eq!(
            sessions.len(),
            1,
            "a duplicate session was launched: {sessions:?}"
        );
        assert_eq!(sessions[0].id, info.id);

        state.agents.close(&info.id).ok();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tasks_list_omits_output_and_honours_limit_and_get_returns_it() {
        let dir = temp_dir("tasks-list-get");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let state = test_state(&dir, &tasks_dir).await;

        let make = |name: &str| Task {
            id: Uuid::new_v4(),
            name: name.to_string(),
            status: TaskStatus::Succeeded,
            input: serde_json::json!({}),
            output: Some(serde_json::json!({ "output": "x".repeat(20_000) })),
            dedupe_key: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: Some(Utc::now()),
            error: None,
            session_id: None,
            session_title: None,
            parent_id: None,
            root_id: None,
        };
        let first = make("one");
        let second = make("two");
        db::upsert_task(&state.db, &first).await.unwrap();
        db::upsert_task(&state.db, &second).await.unwrap();

        // `tasks.list` carries metadata only.
        let resp = dispatch(
            &state,
            Request {
                id: 1,
                method: method::TASKS_LIST.to_string(),
                params: serde_json::json!({}),
            },
        )
        .await;
        let list = resp.result.expect("tasks.list result");
        let list = list.as_array().expect("array");
        assert_eq!(list.len(), 2);
        assert!(
            list.iter().all(|t| t.get("output").is_none()),
            "list must omit output: {list:?}"
        );

        // `limit` narrows the list.
        let resp = dispatch(
            &state,
            Request {
                id: 2,
                method: method::TASKS_LIST.to_string(),
                params: serde_json::json!({ "limit": 1 }),
            },
        )
        .await;
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 1);

        // `tasks.get` returns the full output blob.
        let resp = dispatch(
            &state,
            Request {
                id: 3,
                method: method::TASKS_GET.to_string(),
                params: serde_json::json!({ "id": first.id }),
            },
        )
        .await;
        assert!(resp.result.unwrap().get("output").is_some());

        // Unknown ids are an invalid-params error.
        let resp = dispatch(
            &state,
            Request {
                id: 4,
                method: method::TASKS_GET.to_string(),
                params: serde_json::json!({ "id": Uuid::new_v4() }),
            },
        )
        .await;
        assert_eq!(resp.error.unwrap().code, error_code::INVALID_PARAMS);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
