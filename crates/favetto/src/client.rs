//! Wire client: connects over a Unix socket or a WebSocket and exposes
//! [`request`](Client::request) plus a push broadcast subscribers can listen to.
//!
//! Shared by the TUI and `mcp serve` — both are just different consumers of the same
//! daemon wire protocol.

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::codec::Framed;

use favetto_core::rpc::{Frame, Notification, Request, RequestId, Response};
use favetto_core::wire::{decode, encode, FrameCodec};

type ClientStream = Pin<Box<dyn Stream<Item = anyhow::Result<Frame>> + Send>>;
type ClientSink = Pin<Box<dyn Sink<Frame, Error = anyhow::Error> + Send>>;

/// Where to connect. Local attach is the Unix socket; remote attach is a WebSocket.
#[derive(Debug, Clone)]
pub enum Transport {
    Unix(PathBuf),
    Ws { url: String, token: Option<String> },
}

#[derive(Clone)]
pub struct Client {
    out_tx: mpsc::Sender<Frame>,
    pending: Arc<Mutex<HashMap<RequestId, oneshot::Sender<Response>>>>,
    next_id: Arc<AtomicU64>,
    push_tx: broadcast::Sender<Notification>,
}

impl Client {
    /// Establish a connection of the given transport kind.
    pub async fn connect(transport: Transport) -> anyhow::Result<Self> {
        let (incoming, outgoing) = match transport {
            Transport::Unix(path) => unix_connect(&path).await?,
            Transport::Ws { url, token } => ws_connect(&url, token.as_deref()).await?,
        };
        Self::from_streams(incoming, outgoing)
    }

    fn from_streams(mut incoming: ClientStream, mut outgoing: ClientSink) -> anyhow::Result<Self> {
        let (out_tx, mut out_rx) = mpsc::channel::<Frame>(256);
        let (push_tx, _) = broadcast::channel::<Notification>(256);
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
        tokio::spawn(async move {
            while let Some(res) = incoming.next().await {
                let frame = match res {
                    Ok(f) => f,
                    Err(_) => break,
                };
                match frame {
                    Frame::Response(resp) => {
                        if let Some(tx) = pending2.lock().unwrap().remove(&resp.id) {
                            let _ = tx.send(resp);
                        }
                    }
                    Frame::Notification(n) => {
                        let _ = push_tx2.send(n);
                    }
                    Frame::Request(_) => {}
                }
            }
        });

        Ok(Self {
            out_tx,
            pending,
            next_id: Arc::new(AtomicU64::new(1)),
            push_tx,
        })
    }

    /// Send a request and await its response (5s timeout).
    pub async fn request(&self, method: &str, params: serde_json::Value) -> anyhow::Result<Response> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let req = Request {
            id,
            method: method.to_string(),
            params,
        };
        self.out_tx
            .send(Frame::Request(req))
            .await
            .context("send request")?;

        match tokio::time::timeout(Duration::from_secs(5), rx).await {
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
    let stream = UnixStream::connect(path).await.context("connect unix socket")?;
    let framed = Framed::new(stream, FrameCodec);
    let (sink, stream) = framed.split();
    let incoming: ClientStream = Box::pin(stream.map(|r| r.map_err(|e| anyhow::anyhow!(e))));
    let outgoing: ClientSink = Box::pin(sink.sink_map_err(|e| anyhow::anyhow!(e)));
    Ok((incoming, outgoing))
}

async fn ws_connect(url: &str, token: Option<&str>) -> anyhow::Result<(ClientStream, ClientSink)> {
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    if url.starts_with("wss://") {
        anyhow::bail!("TLS WebSocket (wss://) is not enabled in M6; use ws:// or the Unix socket");
    }

    let mut builder = http::Request::builder().uri(url);
    if let Some(t) = token {
        builder = builder.header(http::header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let request = builder.body(()).map_err(|e| anyhow::anyhow!("bad URL: {e}"))?;
    let (ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .context("connect websocket")?;
    let (sink, stream) = ws.split();

    let incoming: ClientStream = Box::pin(stream.filter_map(|res| async {
        match res {
            Ok(WsMessage::Binary(b)) => Some(decode(b.as_ref()).map_err(|e| anyhow::anyhow!(e))),
            Ok(WsMessage::Text(t)) => Some(decode(t.as_bytes()).map_err(|e| anyhow::anyhow!(e))),
            Ok(WsMessage::Close(_)) => Some(Err(anyhow::anyhow!("connection closed"))),
            Ok(_) => None,
            Err(e) => Some(Err(anyhow::anyhow!("ws error: {e}"))),
        }
    }));

    let outgoing: ClientSink = Box::pin(
        sink.sink_map_err(|e| anyhow::anyhow!("ws send: {e}"))
            .with(|frame: Frame| async move {
                Ok::<_, anyhow::Error>(WsMessage::Binary(encode(&frame)?.into()))
            }),
    );

    Ok((incoming, outgoing))
}
