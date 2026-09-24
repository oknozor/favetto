//! Wire client: connects over a Unix socket or a WebSocket and exposes
//! [`request`](Client::request) plus a push broadcast subscribers can listen to.

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use parking_lot::Mutex;
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::codec::Framed;

use favetto_core::rpc::{Frame, Notification, Request, RequestId, Response};
use favetto_core::wire::FrameCodec;

use crate::ws;

type ClientStream = Pin<Box<dyn Stream<Item = anyhow::Result<Frame>> + Send>>;
type ClientSink = Pin<Box<dyn Sink<Frame, Error = anyhow::Error> + Send>>;

/// Where to connect. Local attach is the Unix socket; remote attach is a WebSocket.
#[derive(Debug, Clone)]
pub enum Transport {
    Unix(PathBuf),
    Ws { url: String, token: Option<String> },
}

/// Default bound on establishing a transport connection.
///
/// A dead or black-holed peer must not block the caller (notably the TUI event
/// loop) for the OS TCP timeout; the timeout cancels the pending connect.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Resolve where a client should attach.
///
/// Precedence, shared by the TUI, the reference supervisor and `favetto-mcp`:
/// an explicit `--remote` wins, then `$FAVETTO_URL`, then an explicit `--socket`,
/// then the default local socket `/tmp/favetto.sock`.
pub fn resolve_transport(
    remote: Option<String>,
    socket: Option<PathBuf>,
    token_file: Option<PathBuf>,
) -> Transport {
    if let Some(remote) = remote {
        return Transport::Ws {
            url: remote,
            token: read_token(token_file),
        };
    }
    if let Ok(url) = std::env::var("FAVETTO_URL") {
        if !url.is_empty() {
            return Transport::Ws {
                url,
                token: read_token(token_file),
            };
        }
    }
    Transport::Unix(socket.unwrap_or_else(|| PathBuf::from("/tmp/favetto.sock")))
}

/// Read a bearer token for a remote attach, defaulting to the standard token
/// path. A missing/unreadable file yields `None` (the daemon will reject an
/// unauthenticated WebSocket, surfacing the real error to the caller).
pub fn read_token(path: Option<PathBuf>) -> Option<String> {
    let path = path.unwrap_or_else(crate::cli::default_token_path);
    std::fs::read_to_string(path)
        .ok()
        .map(|token| token.trim().to_string())
}

#[derive(Clone)]
pub struct Client {
    out_tx: mpsc::Sender<Frame>,
    pending: Arc<Mutex<HashMap<RequestId, oneshot::Sender<Response>>>>,
    next_id: Arc<AtomicU64>,
    push_tx: broadcast::Sender<Notification>,
    closed_tx: broadcast::Sender<()>,
}

impl Client {
    /// Establish a connection of the given transport kind.
    ///
    /// Bounded by [`CONNECT_TIMEOUT`] so a black-holed peer cannot wedge the
    /// caller. Use [`Client::connect_with_timeout`] to override the bound.
    pub async fn connect(transport: Transport) -> anyhow::Result<Self> {
        Self::connect_with_timeout(transport, CONNECT_TIMEOUT).await
    }

    /// Establish a connection, failing if the transport is not ready within
    /// `timeout`. The connect future is cancelled on timeout, so a stalled
    /// WebSocket handshake or TCP connect cannot outlive the bound.
    pub async fn connect_with_timeout(
        transport: Transport,
        timeout: Duration,
    ) -> anyhow::Result<Self> {
        let (incoming, outgoing) = tokio::time::timeout(timeout, async {
            match transport {
                Transport::Unix(path) => unix_connect(&path).await,
                Transport::Ws { url, token } => ws_connect(&url, token.as_deref()).await,
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("connect timed out after {timeout:?}"))??;
        Self::from_streams(incoming, outgoing)
    }

    /// Build a client over an already-connected framed byte stream.
    ///
    /// This is the in-process counterpart of [`Client::connect`]: an embedder or
    /// test can supply its own `AsyncRead + AsyncWrite` transport (for example a
    /// `tokio::io::duplex`) without going through a Unix socket or WebSocket.
    pub fn connect_io<S>(stream: S) -> anyhow::Result<Self>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
    {
        let framed = Framed::new(stream, FrameCodec);
        let (sink, source) = framed.split();
        let incoming: ClientStream = Box::pin(source.map(|r| r.map_err(|e| anyhow::anyhow!(e))));
        let outgoing: ClientSink = Box::pin(sink.sink_map_err(|e| anyhow::anyhow!(e)));
        Self::from_streams(incoming, outgoing)
    }

    fn from_streams(mut incoming: ClientStream, mut outgoing: ClientSink) -> anyhow::Result<Self> {
        let (out_tx, mut out_rx) = mpsc::channel::<Frame>(256);
        let (push_tx, _) = broadcast::channel::<Notification>(256);
        let (closed_tx, _) = broadcast::channel::<()>(1);
        let pending: Arc<Mutex<HashMap<RequestId, oneshot::Sender<Response>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Writer: drain the outgoing queue into the transport sink.
        tokio::spawn(async move {
            while let Some(frame) = out_rx.recv().await {
                if outgoing.send(frame).await.is_err() {
                    break;
                }
            }
        });

        // Reader: route responses to their pending oneshots, pushes to the broadcast.
        let pending2 = pending.clone();
        let push_tx2 = push_tx.clone();
        let closed_tx2 = closed_tx.clone();
        tokio::spawn(async move {
            while let Some(res) = incoming.next().await {
                let frame = match res {
                    Ok(f) => f,
                    Err(_) => break,
                };
                match frame {
                    Frame::Response(resp) => {
                        if let Some(tx) = pending2.lock().remove(&resp.id) {
                            let _ = tx.send(resp);
                        }
                    }
                    Frame::Notification(n) => {
                        let _ = push_tx2.send(n);
                    }
                    Frame::Request(_) => {}
                }
            }
            // Signal closure so the session loop can reconnect promptly.
            let _ = closed_tx2.send(());
        });

        Ok(Self {
            out_tx,
            pending,
            next_id: Arc::new(AtomicU64::new(1)),
            push_tx,
            closed_tx,
        })
    }

    /// Subscribe to the connection-closed signal (fires once when the reader task
    /// exits, e.g. the daemon closed the socket).
    pub fn closed(&self) -> broadcast::Receiver<()> {
        self.closed_tx.subscribe()
    }

    /// Send a request and await its response (5s timeout).
    pub async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<Response> {
        self.request_with_timeout(method, params, Duration::from_secs(5))
            .await
    }

    /// Send a request and await its response with an explicit timeout.
    pub async fn request_with_timeout(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> anyhow::Result<Response> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(id, tx);
        let req = Request {
            id,
            method: method.to_string(),
            params,
        };
        self.out_tx
            .send(Frame::Request(req))
            .await
            .context("send request")?;

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(_)) => anyhow::bail!("connection closed before response"),
            Err(_) => anyhow::bail!("request timed out"),
        }
    }

    /// Subscribe to server pushes (events, task updates, log lines).
    pub fn subscribe(&self) -> broadcast::Receiver<Notification> {
        self.push_tx.subscribe()
    }
}

async fn unix_connect(path: &PathBuf) -> anyhow::Result<(ClientStream, ClientSink)> {
    let stream = UnixStream::connect(path)
        .await
        .context("connect unix socket")?;
    let framed = Framed::new(stream, FrameCodec);
    let (sink, stream) = framed.split();
    let incoming: ClientStream = Box::pin(stream.map(|r| r.map_err(|e| anyhow::anyhow!(e))));
    let outgoing: ClientSink = Box::pin(sink.sink_map_err(|e| anyhow::anyhow!(e)));
    Ok((incoming, outgoing))
}

/// Connect over a WebSocket. Both `ws://` and `wss://` are supported: the
/// `rustls-tls-webpki-roots` feature lets `connect_async` negotiate TLS for
/// `wss://` URLs, validating the server certificate against the Mozilla root
/// store (invalid certificates are rejected; there is no insecure fallback).
async fn ws_connect(url: &str, token: Option<&str>) -> anyhow::Result<(ClientStream, ClientSink)> {
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let mut builder = http::Request::builder().uri(url);
    if let Some(t) = token {
        builder = builder.header(http::header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let request = builder
        .body(())
        .map_err(|e| anyhow::anyhow!("bad URL: {e}"))?;
    let (ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .context("connect websocket")?;
    let (sink, stream) = ws.split();

    let incoming: ClientStream = Box::pin(stream.filter_map(|res| async {
        match res {
            Ok(WsMessage::Binary(b)) => Some(ws::inbound_binary(b.as_ref()).map_err(Into::into)),
            Ok(WsMessage::Text(t)) => Some(ws::inbound_text(&t).map_err(Into::into)),
            Ok(WsMessage::Close(_)) => Some(Err(ws::closed().into())),
            Ok(_) => None,
            Err(e) => Some(Err(anyhow::anyhow!("ws error: {e}"))),
        }
    }));

    let outgoing: ClientSink = Box::pin(
        sink.sink_map_err(|e| anyhow::anyhow!("ws send: {e}"))
            .with(|frame: Frame| async move {
                Ok::<_, anyhow::Error>(WsMessage::Binary(ws::outbound(&frame)?.into()))
            }),
    );

    Ok((incoming, outgoing))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_takes_precedence_over_socket() {
        let transport = resolve_transport(
            Some("ws://example/rpc".to_string()),
            Some(PathBuf::from("/tmp/x.sock")),
            None,
        );
        match transport {
            Transport::Ws { url, .. } => assert_eq!(url, "ws://example/rpc"),
            other => panic!("expected WebSocket, got {other:?}"),
        }
    }

    #[test]
    fn socket_is_the_local_fallback() {
        std::env::remove_var("FAVETTO_URL");
        let transport = resolve_transport(None, Some(PathBuf::from("/tmp/x.sock")), None);
        match transport {
            Transport::Unix(path) => assert_eq!(path, PathBuf::from("/tmp/x.sock")),
            other => panic!("expected Unix socket, got {other:?}"),
        }
    }

    /// A peer that accepts the TCP connection but never answers the WebSocket
    /// handshake must be cancelled by the connect timeout instead of hanging the
    /// caller until the OS TCP timeout.
    #[tokio::test]
    async fn connect_with_timeout_bounds_a_stalled_handshake() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Accept, then stay silent through the HTTP upgrade.
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                tokio::time::sleep(Duration::from_secs(60)).await;
                drop(stream);
            }
        });

        let started = std::time::Instant::now();
        let result = Client::connect_with_timeout(
            Transport::Ws {
                url: format!("ws://{addr}/rpc"),
                token: None,
            },
            Duration::from_millis(200),
        )
        .await;

        assert!(result.is_err(), "stalled connect should time out");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "connect blocked for {:?}",
            started.elapsed()
        );
    }

    /// `wss://` must reach the real TLS connect path instead of short-circuiting
    /// with the old "not enabled" error, and a peer that accepts TCP but never
    /// completes the TLS handshake must still be bounded by the connect timeout.
    ///
    /// If the TLS feature were missing, `connect_async` would fail immediately
    /// with a "TLS support not compiled in" error instead of stalling; the
    /// timeout firing is therefore what proves a TLS handshake was attempted.
    #[tokio::test]
    async fn connect_with_timeout_attempts_tls_and_bounds_a_stalled_handshake() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Accept the TCP connection, then stay silent through the TLS handshake.
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                tokio::time::sleep(Duration::from_secs(60)).await;
                drop(stream);
            }
        });

        let started = std::time::Instant::now();
        let result = Client::connect_with_timeout(
            Transport::Ws {
                url: format!("wss://{addr}/rpc"),
                token: None,
            },
            Duration::from_millis(200),
        )
        .await;

        let err = match result {
            Ok(_) => panic!("stalled TLS connect should not succeed"),
            Err(err) => err,
        };
        let message = err.to_string();
        assert!(
            message.contains("timed out"),
            "wss:// should attempt a TLS handshake then time out, got: {message}"
        );
        assert!(
            !message.contains("not enabled"),
            "wss:// must not short-circuit before connecting: {message}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "connect blocked for {:?}",
            started.elapsed()
        );
    }
}
