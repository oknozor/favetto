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

use favetto_core::rpc::{Frame, Notification, Request, RequestId, Response, RpcCall};
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

    /// Send a typed request and await its typed result (5s timeout).
    ///
    /// The typed layer is a thin wrapper over [`Client::request`]: params are
    /// serialized to the same JSON/MessagePack representation and the result is
    /// decoded into the method's [`RpcCall::Result`]. A method error becomes an
    /// `anyhow` error carrying the numeric code and message.
    pub async fn call<C: RpcCall>(&self, params: C::Params) -> anyhow::Result<C::Result> {
        self.call_with_timeout::<C>(params, Duration::from_secs(5))
            .await
    }

    /// Send a typed request and await its typed result with an explicit timeout.
    pub async fn call_with_timeout<C: RpcCall>(
        &self,
        params: C::Params,
        timeout: Duration,
    ) -> anyhow::Result<C::Result> {
        let params = serde_json::to_value(params).context("serialize params")?;
        let response = self
            .request_with_timeout(C::METHOD, params, timeout)
            .await?;
        if let Some(error) = response.error {
            anyhow::bail!("rpc error {}: {}", error.code, error.message);
        }
        let result = response
            .result
            .ok_or_else(|| anyhow::anyhow!("rpc returned no result"))?;
        serde_json::from_value(result).context("decode result")
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

/// Normalize a remote WebSocket URL to the daemon's `/rpc` upgrade path.
///
/// `--remote`/`FAVETTO_URL` accept either a base URL (`ws://HOST:7878`,
/// `wss://HOST:7878/`) or an explicit endpoint. When the URL has no path a
/// `/rpc` suffix is appended; an already-present `/rpc` (or any other path, so a
/// reverse-proxy mount is not rewritten) is left untouched. The rewrite is
/// idempotent.
pub fn normalize_ws_url(url: &str) -> String {
    match url.parse::<http::Uri>() {
        Ok(uri)
            if matches!(uri.scheme_str(), Some("ws") | Some("wss"))
                && matches!(uri.path(), "" | "/") =>
        {
            format!("{}/rpc", url.trim_end_matches('/'))
        }
        _ => url.to_string(),
    }
}

/// Derive the HTTP base for the one-off pairing exchange from a remote
/// WebSocket URL: the scheme is swapped for its HTTP equivalent and a trailing
/// `/rpc` path is stripped. Both `ws://HOST:7878` and `ws://HOST:7878/rpc`
/// therefore yield `http://HOST:7878`.
pub fn pair_http_base(url: &str) -> String {
    let http = match url.split_once("://") {
        Some(("wss", rest)) => format!("https://{rest}"),
        Some(("ws", rest)) => format!("http://{rest}"),
        _ => url.to_string(),
    };
    let base = http.trim_end_matches('/');
    base.strip_suffix("/rpc").unwrap_or(base).to_string()
}

/// Connect over a WebSocket. Both `ws://` and `wss://` are supported: the
/// `rustls-tls-webpki-roots` feature lets `connect_async` negotiate TLS for
/// `wss://` URLs, validating the server certificate against the Mozilla root
/// store (invalid certificates are rejected; there is no insecure fallback).
///
/// The URL is normalized with [`normalize_ws_url`] first, so a base URL
/// (`ws://HOST:7878`) targets the daemon's `/rpc` route.
async fn ws_connect(url: &str, token: Option<&str>) -> anyhow::Result<(ClientStream, ClientSink)> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let url = normalize_ws_url(url);
    // `into_client_request` adds the mandatory upgrade headers (`Host`,
    // `Connection`, `Upgrade`, `Sec-WebSocket-*`); a bare `http::Request` is
    // rejected by the server's handshake before any route is reached.
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|e| anyhow::anyhow!("bad URL: {e}"))?;
    if let Some(t) = token {
        let value = http::HeaderValue::from_str(&format!("Bearer {t}"))
            .map_err(|e| anyhow::anyhow!("bad bearer token: {e}"))?;
        request
            .headers_mut()
            .insert(http::header::AUTHORIZATION, value);
    }
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

    #[test]
    fn normalize_ws_url_appends_the_rpc_path() {
        assert_eq!(
            normalize_ws_url("ws://127.0.0.1:7878"),
            "ws://127.0.0.1:7878/rpc"
        );
        assert_eq!(normalize_ws_url("wss://HOST:7878/"), "wss://HOST:7878/rpc");
    }

    #[test]
    fn normalize_ws_url_is_idempotent() {
        assert_eq!(
            normalize_ws_url("ws://127.0.0.1:7878/rpc"),
            "ws://127.0.0.1:7878/rpc"
        );
        assert_eq!(
            normalize_ws_url("wss://HOST:7878/rpc"),
            "wss://HOST:7878/rpc"
        );
        // Any other path (a reverse-proxy mount) is preserved verbatim.
        assert_eq!(
            normalize_ws_url("wss://HOST:7878/favetto"),
            "wss://HOST:7878/favetto"
        );
    }

    #[test]
    fn pair_http_base_swaps_scheme_and_strips_rpc() {
        assert_eq!(
            pair_http_base("ws://127.0.0.1:7878"),
            "http://127.0.0.1:7878"
        );
        assert_eq!(
            pair_http_base("ws://127.0.0.1:7878/rpc"),
            "http://127.0.0.1:7878"
        );
        assert_eq!(pair_http_base("wss://HOST:7878/rpc"), "https://HOST:7878");
        assert_eq!(pair_http_base("wss://HOST:7878/rpc/"), "https://HOST:7878");
    }

    /// A base URL (`ws://HOST:PORT` with no path) must set the HTTP request path
    /// to `/rpc`, matching the daemon's only upgrade route.
    #[tokio::test]
    async fn connect_normalizes_a_base_url_to_the_rpc_path() {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let _ = tx.send(String::from_utf8_lossy(&buf[..n]).to_string());
                // Stay silent: the upgrade never completes, so the connect
                // future is cancelled by its timeout once the path is asserted.
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });

        // The server never answers the upgrade, so this resolves as a timeout;
        // the assertion is on the request the client actually sent.
        let _ = Client::connect_with_timeout(
            Transport::Ws {
                url: format!("ws://{addr}"),
                token: None,
            },
            Duration::from_millis(500),
        )
        .await;

        let head = rx.await.expect("server should receive the upgrade request");
        let request_line = head.lines().next().unwrap_or_default();
        assert!(
            request_line.contains("/rpc"),
            "base URL should be normalized to /rpc, got: {request_line}"
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

    /// A typed `call` serializes its params, sends the method name, and decodes
    /// the typed result; a typed RPC error surfaces with its code.
    #[tokio::test]
    async fn call_round_trips_a_typed_result_and_surfaces_errors() {
        use favetto_core::rpc::{
            PingCall, PingResult, Response, RpcError, TaskIdParams, TasksGetCall,
        };
        use tokio_util::codec::Framed;

        async fn connect(reply: Frame) -> (Client, tokio::task::JoinHandle<Request>) {
            let (a, b) = tokio::io::duplex(4096);
            let (client_out, client_in) = Framed::new(a, FrameCodec).split();
            let incoming: ClientStream =
                Box::pin(client_in.map(|r| r.map_err(|e| anyhow::anyhow!(e))));
            let outgoing: ClientSink = Box::pin(client_out.sink_map_err(|e| anyhow::anyhow!(e)));
            let client = Client::from_streams(incoming, outgoing).unwrap();

            let server = tokio::spawn(async move {
                let mut framed = Framed::new(b, FrameCodec);
                let req = match framed.next().await {
                    Some(Ok(Frame::Request(req))) => req,
                    other => panic!("expected a request, got {other:?}"),
                };
                let mut reply = reply;
                if let Frame::Response(resp) = &mut reply {
                    resp.id = req.id;
                }
                framed.send(reply).await.unwrap();
                req
            });
            (client, server)
        }

        let ok = Frame::Response(Response::ok(
            0,
            serde_json::to_value(PingResult {
                pong: true,
                cwd: "/daemon".to_string(),
            })
            .unwrap(),
        ));
        let (client, server) = connect(ok).await;
        let result: PingResult = client.call::<PingCall>(Default::default()).await.unwrap();
        assert!(result.pong);
        assert_eq!(result.cwd, "/daemon");
        let request = server.await.unwrap();
        assert_eq!(request.method, "system.ping");

        let err = Frame::Response(Response {
            id: 0,
            result: None,
            error: Some(RpcError::InvalidParams("task not found".to_string()).to_object()),
        });
        let (client, server) = connect(err).await;
        let error = client
            .call::<TasksGetCall>(TaskIdParams {
                id: uuid::Uuid::new_v4(),
            })
            .await
            .expect_err("an error response must surface");
        assert!(error.to_string().contains("task not found"), "{error}");
        let request = server.await.unwrap();
        assert_eq!(request.method, "tasks.get");
        assert!(request.params.get("id").is_some());
    }
}
