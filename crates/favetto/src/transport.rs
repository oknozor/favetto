//! The transports that expose the daemon: a Unix domain socket (local attach),
//! a WebSocket endpoint (remote attach), and an HTTP `POST /rpc` request/response
//! endpoint. The socket/WebSocket paths adapt their I/O into the same frame
//! `Stream`/`Sink` and hand off to
//! [`serve_connection`](crate::server::serve_connection); the HTTP path decodes a
//! single frame and calls [`crate::server::dispatch`].

use std::path::Path;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Query, State as AxumState};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::net::UnixListener;
use tokio_util::codec::Framed;

use favetto_core::rpc::Frame;
use favetto_core::wire::{self, FrameCodec, WireError};

use crate::server::{self, BoxIn, BoxOut};
use crate::state::State;
use crate::ws;

/// Serve the wire protocol over a Unix domain socket for local TUI attach.
pub async fn serve_unix(path: &Path, state: Arc<State>) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    tracing::info!(path = %path.display(), "favetto listening on unix socket");

    loop {
        let (stream, _addr) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            let framed = Framed::new(stream, FrameCodec);
            let (sink, stream) = framed.split();
            let incoming: BoxIn = Box::pin(stream.map(|r| r.map_err(WireError::from)));
            let outgoing: BoxOut = Box::pin(sink.sink_map_err(WireError::from));
            server::serve_connection(state, incoming, outgoing).await;
        });
    }
}

/// Serve the daemon's HTTP/WebSocket API surface (axum). The router is assembled
/// by the caller (`daemon::run`) so it can include the webhook routes.
pub async fn serve_http(listen: &str, app: Router) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(addr = %listen, "favetto HTTP/WebSocket API listening");
    axum::serve(listener, app).await?;
    Ok(())
}

/// The `/rpc` transport: WebSocket upgrade (GET) and HTTP request/response
/// (POST), with the frame body limit applied to this route only.
pub fn routes() -> Router<Arc<State>> {
    rpc_routes(wire::MAX_FRAME_LEN)
}

/// [`routes`] with an explicit body limit, so tests can exercise the limit
/// without allocating a 64 MiB body.
pub(crate) fn rpc_routes(limit: usize) -> Router<Arc<State>> {
    Router::new()
        .route("/rpc", get(ws_handler).post(rpc_handler))
        .layer(DefaultBodyLimit::max(limit))
}

/// True when `headers` carries a valid `Authorization: Bearer <token>`.
fn authorized(state: &State, headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| state.token.verify(t))
        .unwrap_or(false)
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response()
}

/// Query parameters accepted on the upgrade, so a browser that cannot set an
/// `Authorization` header can present a ticket from `POST /auth/ticket`.
#[derive(Debug, Default, Deserialize)]
pub struct WsQuery {
    /// A single-use, short-lived ticket (see [`crate::ticket::TicketStore`]).
    #[serde(default)]
    pub ticket: Option<String>,
}

/// Reject the upgrade unless a valid `Authorization: Bearer <token>` header is
/// present, or a `?ticket=<t>` from `POST /auth/ticket` redeems exactly once.
pub async fn ws_handler(
    AxumState(state): AxumState<Arc<State>>,
    headers: HeaderMap,
    Query(query): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    // A present bearer wins: a valid token must not burn a ticket that was
    // passed alongside it.
    let authorized = if crate::ticket::bearer_authorized(&state.token, &headers) {
        true
    } else if let Some(ticket) = query.ticket.as_deref() {
        state.tickets.redeem(ticket).await
    } else {
        false
    };

    if !authorized {
        return unauthorized();
    }

    ws.on_upgrade(move |socket| async move {
        serve_socket(state, socket).await;
    })
}

/// `POST /rpc`: read one MessagePack `Frame::Request` and write one
/// `Frame::Response`, delegating all logic to [`crate::server::dispatch`].
pub async fn rpc_handler(
    AxumState(state): AxumState<Arc<State>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !authorized(&state, &headers) {
        return unauthorized();
    }

    let req = match wire::decode(&body) {
        Ok(Frame::Request(req)) => req,
        // A well-formed frame with no request id has nothing to reply to.
        Ok(_) => return (StatusCode::BAD_REQUEST, "expected a request frame").into_response(),
        Err(_) => return (StatusCode::BAD_REQUEST, "malformed frame").into_response(),
    };

    state.metrics.inc_rpc();
    let resp = server::dispatch(&state, req).await;
    match wire::encode(&Frame::Response(resp)) {
        Ok(bytes) => (
            [(axum::http::header::CONTENT_TYPE, "application/x-msgpack")],
            bytes,
        )
            .into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "failed to encode rpc response");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to encode response",
            )
                .into_response()
        }
    }
}

/// Adapt an axum WebSocket into frame stream/sink halves and serve the connection.
async fn serve_socket(state: Arc<State>, socket: WebSocket) {
    let (sink, stream) = socket.split();

    let incoming: BoxIn = Box::pin(stream.filter_map(|res| async {
        match res {
            Ok(Message::Binary(b)) => Some(ws::inbound_binary(b.as_ref())),
            Ok(Message::Text(t)) => Some(ws::inbound_text(&t)),
            Ok(Message::Close(_)) => Some(Err(ws::closed())),
            Ok(_) => None,
            Err(e) => Some(Err(WireError::Other(e.to_string()))),
        }
    }));

    let outgoing: BoxOut = Box::pin(
        sink.sink_map_err(|e| WireError::Other(e.to_string()))
            .with(|frame: Frame| async move {
                Ok::<_, WireError>(Message::Binary(ws::outbound(&frame)?.into()))
            }),
    );

    server::serve_connection(state, incoming, outgoing).await;
}
