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
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

use favetto_core::model::{AgentCatalogEntry, AgentSessionInfo};
use favetto_core::rpc::{error_code, method, push, Frame, Notification, Request, Response};
use favetto_core::wire::WireError;

use crate::agents::{AgentEvent, Invocation};
use crate::db;
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
                    tracing::warn!(n, "client fell behind; events skipped (resume via last_event_id)");
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
        method::PING => Ok(serde_json::json!({ "pong": true })),

        method::TASKS_LIST => match db::list_tasks(&state.db).await {
            Ok(tasks) => Ok(serde_json::json!(tasks)),
            Err(e) => Err((error_code::INTERNAL, e.to_string())),
        },

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
            let input = req.params.get("input").cloned().unwrap_or(serde_json::Value::Null);
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
                        "prompt": d.prompt,
                    })
                })
                .collect();
            Ok(serde_json::json!(list))
        }

        method::CATALOG_ADD => match add_catalog_task(state, &req.params) {
            Ok(def) => Ok(serde_json::json!({
                "name": def.name,
                "agent": def.agent,
                "provider": def.provider,
                "model": def.model,
                "cwd": def.cwd,
                "schedule": def.schedule,
                "needs": def.needs,
            })),
            Err(e) => Err((error_code::INTERNAL, e.to_string())),
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
            let cfg = state.config.read().unwrap();
            let default = cfg.agent.default.clone();
            let mut entries: Vec<AgentCatalogEntry> = cfg
                .agents
                .iter()
                .map(|(name, a)| AgentCatalogEntry {
                    name: name.clone(),
                    command: a.command.clone(),
                    default: default.as_deref() == Some(name.as_str()),
                    sessions: sessions
                        .iter()
                        .filter(|s| &s.agent == name)
                        .cloned()
                        .collect(),
                })
                .collect();
            // Surface sessions whose configured agent was later removed.
            for s in &sessions {
                if !entries.iter().any(|e| e.name == s.agent) {
                    entries.push(AgentCatalogEntry {
                        name: s.agent.clone(),
                        command: String::new(),
                        default: false,
                        sessions: vec![s.clone()],
                    });
                }
            }
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(serde_json::json!(entries))
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
            let rows = req.params.get("rows").and_then(|v| v.as_u64()).unwrap_or(24) as u16;
            let cols = req.params.get("cols").and_then(|v| v.as_u64()).unwrap_or(80) as u16;
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
                None => return Response::err(req.id, error_code::INVALID_PARAMS, "missing channel"),
            };
            let config = req.params.get("config").cloned().unwrap_or(serde_json::Value::Null);
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

    let force_new = params
        .get("new")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Resolve the agent in a scoped block so the config read guard is dropped
    // before the `await` below (a std `RwLockReadGuard` is not `Send`).
    let (name, mut agent) = {
        let cfg = state.config.read().unwrap();
        let name = requested
            .or_else(|| cfg.agent.default.clone())
            .ok_or_else(|| {
                anyhow::anyhow!("no agent given and no default configured ([agent].default)")
            })?;
        let agent = cfg
            .agents
            .get(&name)
            .ok_or_else(|| anyhow::anyhow!("agent '{name}' is not configured under [agents.*]"))?
            .clone();
        (name, agent)
    };

    if let Some(cwd) = params.get("cwd").and_then(|v| v.as_str()) {
        agent.cwd = Some(std::path::PathBuf::from(cwd));
    }

    if !force_new {
        let live = task_id
            .as_deref()
            .and_then(|tid| state.agents.find_latest_by_task(tid));

        // Attach directly to a live interactive TUI already bound to the task.
        if let Some(existing) = &live {
            if existing.running && !existing.headless {
                return Ok(existing.clone());
            }
        }

        // Otherwise resume the agent's own session, never the headless run's PTY
        // (whose screen is machine output). The id comes from the request, the
        // task row, or the running run once its output has been parsed.
        if agent.resume_args.is_some() {
            let sid = match session_id.clone() {
                Some(sid) => Some(sid),
                None => match persisted_session_id(state, task_id.as_deref()).await {
                    Some(sid) => Some(sid),
                    None => wait_for_run_session_id(state, task_id.as_deref()).await,
                },
            };
            if let Some(sid) = sid {
                return state.agents.start(
                    &name,
                    &agent,
                    task_id,
                    Invocation::Resume(&sid),
                    rows,
                    cols,
                );
            }
        }

        // Legacy: replay a finished non-headless session's screen.
        if let Some(existing) = &live {
            if !existing.headless {
                return Ok(existing.clone());
            }
        }
    }

    state
        .agents
        .start(&name, &agent, task_id, Invocation::Interactive(prompt.as_deref()), rows, cols)
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

/// Create or update a schedule: (re)register its cron job and persist it.
async fn upsert_schedule(state: &Arc<State>, params: &serde_json::Value) -> anyhow::Result<favetto_core::model::Schedule> {
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
    let input = params.get("input").cloned().unwrap_or(serde_json::Value::Null);
    let enabled = params.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);

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
fn add_catalog_task(state: &Arc<State>, params: &serde_json::Value) -> anyhow::Result<crate::tasks::TaskDef> {
    use crate::tasks::TaskDef;

    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'name'"))?
        .to_string();
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
    let schedule = params.get("schedule").and_then(|v| v.as_str()).map(str::to_string);
    let needs = params.get("needs").and_then(|v| v.as_str()).map(str::to_string);
    let prompt = params.get("prompt").and_then(|v| v.as_str()).unwrap_or_default().to_string();

    let def = TaskDef { name, agent, provider, model, cwd, schedule, needs, prompt };
    crate::tasks::write_task_md(&state.tasks_dir, &def)?;

    // Update the live catalog.
    {
        let mut catalog = state.catalog.write().unwrap();
        catalog.retain(|d| d.name != def.name);
        catalog.push(def.clone());
        catalog.sort_by(|a, b| a.name.cmp(&b.name));
    }

    Ok(def)
}

async fn delete_schedule(state: &Arc<State>, id: &str) -> anyhow::Result<()> {
    if let Some(job_id) = db::get_schedule_job_id(&state.db, id).await? {
        if let Ok(job_uuid) = Uuid::parse_str(&job_id) {
            let _ = crate::scheduler::unregister(&state.scheduler, &job_uuid).await;
        }
    }
    db::delete_schedule(&state.db, id).await?;
    Ok(())
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
    let config = params.get("config").cloned().unwrap_or(serde_json::Value::Null);

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
        .publish(crate::event_bus::ServerPush::TaskUpdated(task.clone()));

    let event = favetto_core::model::Event {
        id: 0,
        kind: favetto_core::model::EventKind::TaskCancelled,
        payload: serde_json::json!({ "task_id": task.id }),
        created_at: chrono::Utc::now(),
    };
    if let Ok(event_id) = db::insert_event(&state.db, &event).await {
        let event = favetto_core::model::Event { id: event_id, ..event };
        state.bus.publish(crate::event_bus::ServerPush::Event(event));
    }

    Ok(task)
}

/// Build a log-line push (used by later milestones; wired up for completeness).
#[allow(dead_code)]
pub fn log_line(level: &str, message: impl Into<String>) -> Notification {
    Notification {
        method: push::LOG_LINE.to_string(),
        params: serde_json::json!({ "level": level, "message": message.into() }),
    }
}
