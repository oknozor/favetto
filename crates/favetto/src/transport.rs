//! The two transports that expose the daemon: a Unix domain socket (local attach)
//! and a WebSocket endpoint (remote attach). Both adapt their I/O into the same
//! frame `Stream`/`Sink` and hand off to [`serve_connection`](crate::server::serve_connection).

use std::path::Path;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State as AxumState;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use tokio::net::UnixListener;
use tokio_util::codec::Framed;

use favetto_core::rpc::Frame;
use favetto_core::wire::{FrameCodec, WireError};

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

/// Reject the upgrade unless the `Authorization: Bearer <token>` header verifies.
pub async fn ws_handler(
    AxumState(state): AxumState<Arc<State>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let authorized = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| state.token.verify(t))
        .unwrap_or(false);

    if !authorized {
        return (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response();
    }

    ws.on_upgrade(move |socket| async move {
        serve_socket(state, socket).await;
    })
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
