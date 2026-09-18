//! Request dispatch and the connection lifecycle shared by both transports.
//!
//! `serve_connection` is transport-agnostic: it takes a `Stream` of incoming frames
//! and a `Sink` for outgoing frames. The Unix-socket and WebSocket transports are
//! just different ways of producing those two halves, which is why local attach is
//! merely a special case of remote attach.

use std::pin::Pin;
use std::sync::Arc;

use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

use favetto_core::rpc::{error_code, method, push, Frame, Notification, Request, Response};
use favetto_core::wire::WireError;

use crate::db;
use crate::state::State;

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
                // Long-running requests (chat.send streams a full LLM turn) must not
                // block the read loop, otherwise the client's pings time out and the
                // connection flaps. Run them on a separate task.
                if req.method == method::CHAT_SEND {
                    let state = state.clone();
                    let out_tx = out_tx.clone();
                    tokio::spawn(async move {
                        handle_request(&state, &out_tx, req).await;
                    });
                } else {
                    handle_request(&state, &out_tx, req).await;
                }
            }
            Frame::Notification(_) | Frame::Response(_) => {}
        }
    }

    drop(out_tx);
    pusher.abort();
    let _ = writer.await;
}

/// Route a single request. `events.subscribe` is handled here because it produces
/// an ack plus a variable number of replayed events.
async fn handle_request(state: &Arc<State>, out_tx: &mpsc::Sender<Frame>, req: Request) {
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

    // `chat.send` streams answer/reasoning tokens as pushes before the final
    // response, so it's handled here rather than in `dispatch`.
    if req.method == method::CHAT_SEND {
        let session_id = match req.params.get("session_id").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => {
                let _ = out_tx
                    .send(Frame::Response(Response::err(
                        id,
                        error_code::INVALID_PARAMS,
                        "missing session_id",
                    )))
                    .await;
                return;
            }
        };
        let message = match req.params.get("message").and_then(|v| v.as_str()) {
            Some(m) => m.to_string(),
            None => {
                let _ = out_tx
                    .send(Frame::Response(Response::err(
                        id,
                        error_code::INVALID_PARAMS,
                        "missing message",
                    )))
                    .await;
                return;
            }
        };

        let sid = session_id.clone();
        let delta_tx = out_tx.clone();
        let on_delta = move |s: &str| {
            let _ = delta_tx.try_send(Frame::Notification(Notification {
                method: push::CHAT_DELTA.to_string(),
                params: serde_json::json!({ "session_id": sid, "text": s }),
            }));
        };
        let sid = session_id.clone();
        let reason_tx = out_tx.clone();
        let on_reasoning = move |s: &str| {
            let _ = reason_tx.try_send(Frame::Notification(Notification {
                method: push::CHAT_REASONING.to_string(),
                params: serde_json::json!({ "session_id": sid, "text": s }),
            }));
        };

        let resp = match state
            .chat
            .send_streaming(&session_id, &message, &on_delta, &on_reasoning)
            .await
        {
            Ok(session) => Response::ok(id, serde_json::json!(session)),
            Err(e) => Response::err(id, error_code::INTERNAL, e.to_string()),
        };
        let _ = out_tx.send(Frame::Response(resp)).await;
        return;
    }

    let resp = dispatch(state, req).await;
    let _ = out_tx.send(Frame::Response(resp)).await;
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
                        "model": d.model,
                        "schedule": d.schedule,
                        "needs": d.needs,
                    })
                })
                .collect();
            Ok(serde_json::json!(list))
        }

        method::CATALOG_ADD => match add_catalog_task(state, &req.params) {
            Ok(def) => Ok(serde_json::json!({
                "name": def.name,
                "model": def.model,
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

        method::CHAT_OPEN => {
            let task_id = req.params.get("task_id").and_then(|v| v.as_str()).map(str::to_string);
            match state.chat.open(&state.db, task_id).await {
                Ok(session) => Ok(serde_json::json!(session)),
                Err(e) => Err((error_code::INTERNAL, e.to_string())),
            }
        }

        method::CHAT_MESSAGES => {
            let session_id = match req.params.get("session_id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return Response::err(req.id, error_code::INVALID_PARAMS, "missing session_id"),
            };
            match state.chat.messages(&session_id).await {
                Ok(session) => Ok(serde_json::json!(session)),
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

        method::CONFIG_SET_PROVIDER => match set_provider(state, &req.params).await {
            Ok(provider) => Ok(serde_json::json!(provider)),
            Err(e) => Err((error_code::INTERNAL, e.to_string())),
        },

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
    let model = params
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'model'"))?
        .to_string();
    let schedule = params.get("schedule").and_then(|v| v.as_str()).map(str::to_string);
    let needs = params.get("needs").and_then(|v| v.as_str()).map(str::to_string);
    let prompt = params.get("prompt").and_then(|v| v.as_str()).unwrap_or_default().to_string();

    let def = TaskDef { name, model, schedule, needs, prompt };
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

/// Add or update an LLM provider: mutate the live config and persist it to disk.
async fn set_provider(state: &Arc<State>, params: &serde_json::Value) -> anyhow::Result<crate::config::Provider> {
    use crate::config::Provider;

    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'name'"))?
        .to_string();
    let provider = Provider {
        kind: params
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("openai")
            .to_string(),
        api_key: params.get("api_key").and_then(|v| v.as_str()).map(str::to_string),
        api_key_env: params.get("api_key_env").and_then(|v| v.as_str()).map(str::to_string),
        model: params.get("model").and_then(|v| v.as_str()).map(str::to_string),
        base_url: params.get("base_url").and_then(|v| v.as_str()).map(str::to_string),
    };

    let toml = {
        let mut cfg = state.config.write().unwrap();
        cfg.providers.insert(name.clone(), provider.clone());
        cfg.to_toml()?
    };
    if let Err(e) = tokio::fs::write(&state.config_path, toml).await {
        tracing::warn!(error = %e, path = %state.config_path.display(), "failed to persist config");
    }

    Ok(provider)
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
