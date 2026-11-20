//! The tokio runtime thread and its GPUI channel bridge.
//!
//! `reqwest` and the SSE reader are async; GPUI's UI thread is not a tokio
//! runtime. The desktop client therefore owns a dedicated tokio runtime on a
//! background thread and talks to it over two `async-channel`s:
//!
//! ```text
//!   UI thread (GPUI)                    runtime thread (tokio)
//!   ────────────────                    ─────────────────────
//!   Command::Ping / Reconnect  ───────▶  RPC calls, SSE stream
//!   Update::Connection / Ping / Push ◀─  (decoded, typed values)
//! ```
//!
//! Commands are best-effort: a full command queue drops a duplicate ping or
//! reconnect rather than blocking the UI. Updates are bounded too — a UI that
//! stops draining loses old pushes, which the durable event cursor can replay.

#![allow(dead_code)] // `ping()` and the push accessors are consumed by steps 03+.

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use async_channel::{Receiver, Sender};
use favetto_wire::rpc::{PingParams, PingResult};

use crate::config::ClientConfig;
use crate::sse::{self, Push, SseEvent, SseStatus};
use crate::state::ConnectionState;
use crate::transport::{ClientError, RpcClient};

/// Work the UI asks the runtime to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Run one `system.ping` round trip.
    Ping,
    /// Drop the SSE stream, start a fresh one, and re-validate the connection.
    Reconnect,
    /// Stop the worker thread.
    Shutdown,
}

/// The result of one `system.ping` round trip, with its wall-clock latency.
#[derive(Debug, Clone)]
pub struct PingSnapshot {
    pub result: Result<PingResult, ClientError>,
    pub round_trip: Duration,
}

/// Values the runtime sends back to the UI.
#[derive(Debug, Clone)]
pub enum Update {
    /// The connection-state transitioned (SSE status or a transport failure).
    Connection(ConnectionState),
    /// A `system.ping` round trip completed.
    Ping(PingSnapshot),
    /// One decoded server push.
    Push(Box<Push>),
    /// The durable resume cursor advanced.
    Cursor(Option<i64>),
    /// A push whose payload could not be decoded (shown, never fatal).
    Malformed(String),
}

/// The runtime thread handle plus both ends of the bridge.
pub struct Runtime {
    commands: Sender<Command>,
    updates: Receiver<Update>,
    thread: Option<JoinHandle<()>>,
}

impl Runtime {
    /// Start the tokio thread.
    ///
    /// Fails only when the OS refuses to spawn the thread; connection errors are
    /// reported later as [`Update::Connection`] values.
    pub fn spawn(config: ClientConfig) -> Result<Self, String> {
        let (commands, command_rx) = async_channel::bounded(16);
        let (update_tx, updates) = async_channel::bounded(512);

        let thread = std::thread::Builder::new()
            .name("favetto-gpui-runtime".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .thread_name("favetto-gpui-tokio")
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(err) => {
                        let _ =
                            update_tx.try_send(Update::Connection(ConnectionState::Disconnected {
                                message: format!("could not start the tokio runtime: {err}"),
                            }));
                        return;
                    }
                };
                runtime.block_on(worker(config, command_rx, update_tx));
            })
            .map_err(|err| format!("could not start the runtime thread: {err}"))?;

        Ok(Self {
            commands,
            updates,
            thread: Some(thread),
        })
    }

    /// Queue a command without blocking the UI.
    pub fn send(&self, command: Command) {
        let _ = self.commands.try_send(command);
    }

    /// A clone of the update stream for the foreground drain loop.
    pub fn updates(&self) -> Receiver<Update> {
        self.updates.clone()
    }

    /// Ask for one ping round trip.
    pub fn ping(&self) {
        self.send(Command::Ping);
    }

    /// Restart the SSE stream and re-validate the connection.
    pub fn reconnect(&self) {
        self.send(Command::Reconnect);
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // Use the blocking send so Shutdown cannot be dropped by a full queue
        // (which would make the `join` below wait forever).
        let _ = self.commands.send_blocking(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            // The worker aborts the SSE task on shutdown, so this is prompt.
            let _ = thread.join();
        }
    }
}

/// The runtime thread's main loop.
async fn worker(config: ClientConfig, commands: Receiver<Command>, updates: Sender<Update>) {
    let _ = updates.try_send(Update::Connection(ConnectionState::Connecting));

    let client = match RpcClient::new(&config) {
        Ok(client) => Arc::new(client),
        Err(err) => {
            let _ = updates.try_send(Update::Connection(ConnectionState::Disconnected {
                message: err.to_string(),
            }));
            return;
        }
    };

    let token_hint = config.token_source.label();
    let mut sse_task = Some(spawn_sse(&config, &updates, token_hint.clone()));
    spawn_ping(client.clone(), updates.clone());

    while let Ok(command) = commands.recv().await {
        match command {
            Command::Shutdown => break,
            Command::Ping => spawn_ping(client.clone(), updates.clone()),
            Command::Reconnect => {
                if let Some(task) = sse_task.take() {
                    task.abort();
                }
                let _ = updates.try_send(Update::Connection(ConnectionState::Connecting));
                sse_task = Some(spawn_sse(&config, &updates, token_hint.clone()));
                spawn_ping(client.clone(), updates.clone());
            }
        }
    }

    if let Some(task) = sse_task {
        task.abort();
    }
}

/// Spawn one ping round trip; the reply is an [`Update::Ping`].
fn spawn_ping(client: Arc<RpcClient>, updates: Sender<Update>) {
    tokio::spawn(async move {
        let started = Instant::now();
        let result = client.ping(PingParams {}).await;
        let snapshot = PingSnapshot {
            result,
            round_trip: started.elapsed(),
        };
        let _ = updates.try_send(Update::Ping(snapshot));
    });
}

/// Spawn the SSE reader, wiring its callbacks to the update channel.
fn spawn_sse(
    config: &ClientConfig,
    updates: &Sender<Update>,
    token_hint: String,
) -> tokio::task::JoinHandle<()> {
    let url = config.events_url();
    let token = config.token.clone();
    let push_tx = updates.clone();
    let status_tx = updates.clone();

    tokio::spawn(async move {
        let mut sent_cursor: Option<i64> = None;
        sse::run(
            url,
            token,
            None,
            move |event: SseEvent, cursor: Option<i64>| {
                if cursor != sent_cursor {
                    sent_cursor = cursor;
                    let _ = push_tx.try_send(Update::Cursor(cursor));
                }
                match sse::decode_push(&event) {
                    Ok(Some(push)) => {
                        let _ = push_tx.try_send(Update::Push(Box::new(push)));
                    }
                    Ok(None) => {}
                    Err(err) => {
                        let _ = push_tx.try_send(Update::Malformed(err.to_string()));
                    }
                }
            },
            move |status| {
                let connection = connection_for(status, &token_hint);
                let _ = status_tx.try_send(Update::Connection(connection));
            },
        )
        .await;
    })
}

/// Map an SSE transition to the UI's connection state.
fn connection_for(status: SseStatus, token_path: &str) -> ConnectionState {
    match status {
        SseStatus::Connected => ConnectionState::Connected,
        SseStatus::Reconnecting { attempt, delay_ms } => {
            ConnectionState::Reconnecting { attempt, delay_ms }
        }
        SseStatus::Unauthorized => ConnectionState::Unauthorized {
            token_path: token_path.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_statuses_map_to_connection_states() {
        assert_eq!(
            connection_for(SseStatus::Connected, "token"),
            ConnectionState::Connected
        );
        assert_eq!(
            connection_for(
                SseStatus::Reconnecting {
                    attempt: 2,
                    delay_ms: 4_000
                },
                "token"
            ),
            ConnectionState::Reconnecting {
                attempt: 2,
                delay_ms: 4_000
            }
        );
        assert_eq!(
            connection_for(
                SseStatus::Unauthorized,
                "/home/u/.local/share/favetto/token"
            ),
            ConnectionState::Unauthorized {
                token_path: "/home/u/.local/share/favetto/token".to_string()
            }
        );
    }
}
