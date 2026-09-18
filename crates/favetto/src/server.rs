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
            Frame::Request(req) => handle_request(&state, &out_tx, req).await,
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

        method::CHAT_SEND => {
            let session_id = match req.params.get("session_id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return Response::err(req.id, error_code::INVALID_PARAMS, "missing session_id"),
            };
            let message = match req.params.get("message").and_then(|v| v.as_str()) {
                Some(m) => m.to_string(),
                None => return Response::err(req.id, error_code::INVALID_PARAMS, "missing message"),
            };
            match state.chat.send(&session_id, &message).await {
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
    let skill = params
        .get("skill")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing skill"))?
        .to_string();
    let input = params.get("input").cloned().unwrap_or(serde_json::Value::Null);
    let enabled = params.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);

    let schedule = Schedule {
        id,
        cron,
        skill,
        input,
        enabled,
        last_run: None,
    };

    // Remove any existing job, then register the new one.
    if let Some(old_job_id) = db::get_schedule_job_id(&state.db, &schedule.id).await? {
        if let Ok(job_uuid) = Uuid::parse_str(&old_job_id) {
            let _ = crate::scheduler::unregister(&state.scheduler, &job_uuid).await;
        }
    }

    db::upsert_schedule(&state.db, &schedule).await?;

    if schedule.enabled {
        let job_id = crate::scheduler::register(&state.scheduler, state.clone(), &schedule).await?;
        db::set_schedule_job_id(&state.db, &schedule.id, &job_id.to_string()).await?;
    }

    Ok(schedule)
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
