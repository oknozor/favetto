//! End-to-end ACP turn cancellation (`agents.cancel`).
//!
//! Spawns the real daemon against the permission-blocking fake ACP agent and
//! verifies that cancelling a turn:
//!
//! - answers the outstanding `session/request_permission` with the ACP
//!   `cancelled` outcome instead of waiting for the 300-second timeout;
//! - sends exactly one `session/cancel` for each active turn, so successive
//!   prompt/cancel cycles work over one connection;
//! - does not send a notification when the session is already idle;
//! - settles a permission answer racing the cancel without double-resolving it.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use favetto_core::rpc::method;
use favetto_tui::client::{Client, Transport};
use serde_json::{json, Value};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
/// A cancellation must settle long before ACP's 300-second permission timeout.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(25);

/// An isolated daemon plus its permission-blocking fake agent.
struct Daemon {
    root: PathBuf,
    socket: PathBuf,
    marker: PathBuf,
    child: Child,
}

impl Daemon {
    fn spawn() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "favetto-agent-cancel-{}-{nanos}",
            std::process::id()
        ));
        let data_dir = root.join("data");
        let tasks_dir = root.join("tasks");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&tasks_dir).unwrap();

        let marker = root.join("cancel-marker");
        let agent = env!("CARGO_BIN_EXE_fake-acp-agent");
        let config_path = root.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\ndefault = \"perm\"\n\n\
                 [agents.perm]\ncommand = {:?}\nargs = []\n\
                 [agents.perm.env]\n\
                 FAKE_ACP_PERMISSION = \"1\"\n\
                 FAKE_ACP_CANCEL_MARKER = {:?}\n\
                 FAKE_ACP_IGNORE_CLOSE = \"1\"\n",
                agent, marker
            ),
        )
        .unwrap();

        let socket = root.join("favetto.sock");
        let log = std::fs::File::create(root.join("daemon.log")).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_favetto"))
            .arg("daemon")
            .arg("--config")
            .arg(&config_path)
            .arg("--data-dir")
            .arg(&data_dir)
            .arg("--tasks-dir")
            .arg(&tasks_dir)
            .arg("--socket")
            .arg(&socket)
            .arg("--listen")
            .arg("127.0.0.1:0")
            .current_dir(&root)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .env_remove("FAVETTO_CONFIG")
            .env_remove("FAVETTO_DATA_DIR")
            .spawn()
            .expect("spawn favetto daemon");

        Self {
            root,
            socket,
            marker,
            child,
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.root.join("daemon.log")).unwrap_or_default()
    }

    fn marker_lines(&self) -> Vec<String> {
        std::fs::read_to_string(&self.marker)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn count(&self, line: &str) -> usize {
        self.marker_lines()
            .iter()
            .filter(|entry| entry.as_str() == line)
            .count()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn wait_for_socket(path: &Path, daemon: &Daemon) {
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        match tokio::net::UnixStream::connect(path).await {
            Ok(_) => return,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => panic!(
                "daemon socket {} never became ready: {error}\n--- daemon log ---\n{}",
                path.display(),
                daemon.log()
            ),
        }
    }
}

/// Issue one RPC and return its result value.
async fn call(client: &Client, method: &str, params: Value) -> Value {
    let response = client
        .request(method, params)
        .await
        .unwrap_or_else(|error| panic!("{method} failed: {error}"));
    assert!(
        response.error.is_none(),
        "{method} returned an error: {:?}",
        response.error
    );
    response.result.unwrap_or(Value::Null)
}

/// Find one live session by its Favetto-local id.
fn session<'a>(list: &'a Value, id: &str) -> Option<&'a Value> {
    list.as_array()?.iter().find_map(|agent| {
        agent["sessions"]
            .as_array()?
            .iter()
            .find(|session| session["id"].as_str() == Some(id))
    })
}

/// Poll `agents.list` until `pred` accepts the session.
async fn wait_session(
    client: &Client,
    daemon: &Daemon,
    id: &str,
    pred: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = tokio::time::Instant::now() + SETTLE_TIMEOUT;
    loop {
        let list = call(client, method::AGENTS_LIST, json!({})).await;
        if let Some(found) = session(&list, id) {
            if pred(found) {
                return found.clone();
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "session '{id}' never reached the expected state\n--- daemon log ---\n{}",
            daemon.log()
        );
        tokio::time::sleep(POLL).await;
    }
}

/// Poll until the fake agent has recorded at least `count` occurrences of `line`.
async fn wait_marker(daemon: &Daemon, line: &str, count: usize) {
    let deadline = tokio::time::Instant::now() + SETTLE_TIMEOUT;
    loop {
        if daemon.count(line) >= count {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "marker '{line}' never reached {count}\n--- marker ---\n{:?}\n--- daemon log ---\n{}",
            daemon.marker_lines(),
            daemon.log()
        );
        tokio::time::sleep(POLL).await;
    }
}

/// Wait until a cancelled turn has settled back to idle.
async fn wait_idle(client: &Client, daemon: &Daemon, id: &str) -> Value {
    wait_session(client, daemon, id, |session| {
        session["awaiting_input"].is_null() && session["activity"]["kind"] == "idle"
    })
    .await
}

/// Send a prompt and wait for the fake agent's permission request.
async fn prompt_until_permission(client: &Client, daemon: &Daemon, id: &str) {
    call(
        client,
        method::AGENTS_PROMPT,
        json!({ "session_id": id, "prompt": "do work" }),
    )
    .await;
    wait_session(client, daemon, id, |session| {
        !session["awaiting_input"].is_null()
    })
    .await;
}

#[tokio::test]
async fn cancelling_a_turn_settles_pending_permissions_and_can_repeat() {
    let daemon = Daemon::spawn();
    wait_for_socket(&daemon.socket, &daemon).await;
    let client = Client::connect(Transport::Unix(daemon.socket.clone()))
        .await
        .expect("connect to the daemon");
    let cwd = daemon.root.to_string_lossy().into_owned();

    let started = call(
        &client,
        method::AGENTS_START,
        json!({ "agent": "perm", "cwd": cwd }),
    )
    .await;
    let id = started["session"]["id"]
        .as_str()
        .expect("Favetto-local session id")
        .to_string();
    wait_session(&client, &daemon, &id, |session| {
        session["running"] == json!(true) && session["session_id"].is_string()
    })
    .await;

    // Turn one: cancel while the permission request is pending. The reply must
    // be the ACP `cancelled` outcome, and the turn must settle promptly.
    prompt_until_permission(&client, &daemon, &id).await;
    let cancelled = call(&client, method::AGENTS_CANCEL, json!({ "session_id": id })).await;
    assert_eq!(cancelled["cancelled"], json!(true));
    wait_marker(&daemon, "outcome cancelled", 1).await;
    wait_idle(&client, &daemon, &id).await;

    // Turn two on the same connection proves cancellation is per-turn, not a
    // connection-lifetime latch.
    prompt_until_permission(&client, &daemon, &id).await;
    call(&client, method::AGENTS_CANCEL, json!({ "session_id": id })).await;
    wait_marker(&daemon, "outcome cancelled", 2).await;
    wait_idle(&client, &daemon, &id).await;
    assert_eq!(
        daemon.count("cancel"),
        2,
        "each turn must send exactly one cancel notification: {:?}",
        daemon.marker_lines()
    );

    // Idle: a cancel is a no-op and never reaches the agent.
    let idle_cancels = daemon.count("cancel");
    call(&client, method::AGENTS_CANCEL, json!({ "session_id": id })).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        daemon.count("cancel"),
        idle_cancels,
        "an idle session must not send session/cancel: {:?}",
        daemon.marker_lines()
    );

    // Turn three: a client answer races the cancel. Whichever wins, the request
    // resolves exactly once and the connection remains usable.
    prompt_until_permission(&client, &daemon, &id).await;
    let session = wait_session(&client, &daemon, &id, |session| {
        !session["awaiting_input"].is_null()
    })
    .await;
    let request_id = session["awaiting_input"]["request_id"]
        .as_str()
        .expect("permission request id")
        .to_string();
    call(
        &client,
        method::AGENTS_REPLY,
        json!({
            "session_id": id,
            "request_id": request_id,
            "reply": { "reply": "once" }
        }),
    )
    .await;
    call(&client, method::AGENTS_CANCEL, json!({ "session_id": id })).await;
    wait_idle(&client, &daemon, &id).await;
    assert!(
        daemon.count("outcome selected") == 1,
        "the client answer should have produced one selected outcome: {:?}",
        daemon.marker_lines()
    );

    // Turn four: the connection still cancels after the race.
    let before = daemon.count("outcome cancelled");
    prompt_until_permission(&client, &daemon, &id).await;
    call(&client, method::AGENTS_CANCEL, json!({ "session_id": id })).await;
    wait_marker(&daemon, "outcome cancelled", before + 1).await;
    wait_idle(&client, &daemon, &id).await;

    let _ = call(&client, method::AGENTS_CLOSE, json!({ "session_id": id })).await;
}
