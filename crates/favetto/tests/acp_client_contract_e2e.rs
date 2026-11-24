//! Client-side ACP wire contract (`acp.rs` as the ACP **client**).
//!
//! The official acp-tck only drives ACP *agents*, so it cannot score Favetto's
//! client role. These tests close that gap for the wire level: the fake agent
//! records every client→agent message (`FAKE_ACP_TRANSCRIPT`) and the tests
//! assert what Favetto actually sends — the `initialize` handshake and the
//! advertised `clientCapabilities`, `session/new` params, `session/prompt`
//! content, `session/cancel` + the `cancelled` permission outcome on a
//! cancelled turn, and that no unadvertised methods (`fs/*`, `terminal/*`,
//! `elicitation/*`) are ever called.
//!
//! The agent-side counterpart (the fake agent itself against the acp-tck) is
//! gated in CI by `.github/workflows/acp-tck.yml`.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use favetto_core::rpc::method;
use favetto_tui::client::{Client, Transport};
use serde_json::{json, Value};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const SETTLE_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(25);

/// An isolated daemon plus its transcript-recording fake ACP agents.
struct Daemon {
    root: PathBuf,
    socket: PathBuf,
    transcript: PathBuf,
    child: Child,
}

impl Daemon {
    fn spawn() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "favetto-acp-contract-{}-{nanos}",
            std::process::id()
        ));
        let data_dir = root.join("data");
        let tasks_dir = root.join("tasks");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&tasks_dir).unwrap();

        let transcript = root.join("acp-transcript.jsonl");
        let agent = env!("CARGO_BIN_EXE_fake-acp-agent");
        let config_path = root.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\ndefault = \"echo\"\n\n\
                 [agents.echo]\ncommand = {agent:?}\nargs = []\n\
                 [agents.echo.env]\nFAKE_ACP_TRANSCRIPT = {transcript:?}\n\n\
                 [agents.perm]\ncommand = {agent:?}\nargs = []\n\
                 [agents.perm.env]\n\
                 FAKE_ACP_PERMISSION = \"1\"\n\
                 FAKE_ACP_TRANSCRIPT = {transcript:?}\n"
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
            transcript,
            child,
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.root.join("daemon.log")).unwrap_or_default()
    }

    /// Every client→agent message the fake agent has recorded so far.
    fn transcript(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.transcript)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
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
                tokio::time::sleep(POLL).await;
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

/// Poll `agents.list` until the session satisfies `pred`.
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

/// Poll the transcript until `pred` accepts some message.
async fn wait_transcript(daemon: &Daemon, pred: impl Fn(&Value) -> bool) -> Vec<Value> {
    let deadline = tokio::time::Instant::now() + SETTLE_TIMEOUT;
    loop {
        let messages = daemon.transcript();
        if messages.iter().any(&pred) {
            return messages;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no transcript message matched\n--- transcript ---\n{:?}\n--- daemon log ---\n{}",
            daemon.transcript(),
            daemon.log()
        );
        tokio::time::sleep(POLL).await;
    }
}

/// Methods a conformant Favetto client may send to an agent, given the
/// capabilities it advertises. Notably absent: `fs/*`, `terminal/*`,
/// `elicitation/create` (those are agent→client and Favetto advertises no such
/// client capability).
const ALLOWED_CLIENT_METHODS: &[&str] = &[
    "initialize",
    "authenticate",
    "logout",
    "session/new",
    "session/load",
    "session/resume",
    "session/list",
    "session/delete",
    "session/close",
    "session/prompt",
    "session/cancel",
    "session/set_mode",
    "session/set_config_option",
    "session/options",
    "$/cancel_request",
];

#[tokio::test]
async fn client_handshake_and_prompt_wire_contract() {
    let daemon = Daemon::spawn();
    wait_for_socket(&daemon.socket, &daemon).await;
    let client = Client::connect(Transport::Unix(daemon.socket.clone()))
        .await
        .expect("connect to the daemon");
    let cwd = daemon.root.to_string_lossy().into_owned();

    let started = call(
        &client,
        method::AGENTS_START,
        json!({ "agent": "echo", "cwd": cwd }),
    )
    .await;
    let local_id = started["session"]["id"]
        .as_str()
        .expect("local id")
        .to_string();
    let acp_id = wait_session(&client, &daemon, &local_id, |session| {
        session["running"] == json!(true) && session["session_id"].is_string()
    })
    .await["session_id"]
        .as_str()
        .expect("ACP session id")
        .to_string();

    call(
        &client,
        method::AGENTS_PROMPT,
        json!({ "session_id": local_id, "prompt": "wire-contract-probe" }),
    )
    .await;
    wait_session(&client, &daemon, &local_id, |session| {
        session["awaiting_input"].is_null() && session["activity"]["kind"] == "idle"
    })
    .await;
    // The transcript now holds the whole client→agent exchange. (The
    // `session/close`-when-advertised half of the contract is covered by
    // daemon_e2e's close tests, which watch the close marker on task teardown.)
    let messages = wait_transcript(&daemon, |message| {
        message["method"] == json!("session/prompt")
    })
    .await;

    // `initialize` is the first message on the connection and negotiates v1.
    let initialize = &messages[0];
    assert_eq!(initialize["method"], json!("initialize"));
    assert_eq!(initialize["params"]["protocolVersion"], json!(1));
    // `clientInfo` is optional in v1; when sent it must carry a name.
    if let Some(info) = initialize["params"].get("clientInfo") {
        assert!(
            info["name"].is_string(),
            "clientInfo.name missing: {initialize:?}"
        );
    }

    // Favetto advertises form elicitation plus the boolean config-option gate
    // (ACP-CONFIG-003: agents may then include `type: "boolean"` options). The
    // SDK serializes disabled capabilities as explicit `false`, so "off" means
    // absent or all-false — never an enabled object.
    let caps = &initialize["params"]["clientCapabilities"];
    assert_eq!(caps["elicitation"]["form"], json!({}), "caps: {caps:?}");
    assert!(caps["elicitation"].get("url").is_none(), "caps: {caps:?}");
    assert_eq!(
        caps["session"]["configOptions"]["boolean"],
        json!({}),
        "boolean config gate must be advertised: {caps:?}"
    );
    // A capability is "on" if it is `true` or an object with any on leaf; an
    // empty object (like `form: {}`) counts as on, an all-false object as off.
    fn enabled(value: &Value) -> bool {
        match value {
            Value::Bool(on) => *on,
            Value::Object(map) if map.is_empty() => true,
            Value::Object(map) => map.values().any(enabled),
            _ => false,
        }
    }
    for off in ["fs", "terminal", "subagents"] {
        assert!(
            !enabled(&caps[off]),
            "client capability '{off}' must be off, got: {caps:?}"
        );
    }
    assert!(
        !enabled(&caps["auth"]["terminal"]),
        "terminal auth must stay off: {caps:?}"
    );

    // `session/new` carries cwd and mcpServers, and never the unsupported
    // additionalDirectories / systemPrompt params.
    let new = messages
        .iter()
        .find(|message| message["method"] == json!("session/new"))
        .expect("a session/new request");
    assert_eq!(new["params"]["cwd"], json!(cwd));
    assert!(
        new["params"]["mcpServers"].is_array(),
        "mcpServers: {new:?}"
    );
    assert!(new["params"].get("additionalDirectories").is_none());
    assert!(new["params"].get("systemPrompt").is_none());

    // `session/prompt` names the ACP session id from `session/new` and sends the
    // prompt as text content blocks.
    let prompt = messages
        .iter()
        .find(|message| message["method"] == json!("session/prompt"))
        .expect("a session/prompt request");
    assert_eq!(prompt["params"]["sessionId"], json!(acp_id));
    let blocks = prompt["params"]["prompt"]
        .as_array()
        .expect("prompt blocks");
    assert!(
        blocks.iter().any(|block| block["type"] == json!("text")
            && block["text"]
                .as_str()
                .is_some_and(|text| text.contains("wire-contract-probe"))),
        "prompt text not forwarded: {prompt:?}"
    );

    // The client only ever calls methods it may call.
    for message in &messages {
        let method_name = message["method"].as_str().unwrap_or_default();
        assert!(
            ALLOWED_CLIENT_METHODS.contains(&method_name),
            "client sent an unexpected method '{method_name}': {message:?}"
        );
        assert!(
            !method_name.starts_with("fs/")
                && !method_name.starts_with("terminal/")
                && !method_name.starts_with("elicitation/"),
            "client called an unadvertised agent→client surface: {message:?}"
        );
    }
}

#[tokio::test]
async fn cancel_settles_the_pending_permission_as_cancelled_on_the_wire() {
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
    let local_id = started["session"]["id"]
        .as_str()
        .expect("local id")
        .to_string();
    let acp_id = wait_session(&client, &daemon, &local_id, |session| {
        session["running"] == json!(true) && session["session_id"].is_string()
    })
    .await["session_id"]
        .as_str()
        .expect("ACP session id")
        .to_string();

    call(
        &client,
        method::AGENTS_PROMPT,
        json!({ "session_id": local_id, "prompt": "do work" }),
    )
    .await;
    wait_session(&client, &daemon, &local_id, |session| {
        !session["awaiting_input"].is_null()
    })
    .await;
    let cancelled = call(
        &client,
        method::AGENTS_CANCEL,
        json!({ "session_id": local_id }),
    )
    .await;
    assert_eq!(cancelled["cancelled"], json!(true));

    // On the wire: one `session/cancel` notification for the turn, and the
    // pending `session/request_permission` answered with the `cancelled` outcome.
    wait_transcript(&daemon, |message| {
        message["method"] == json!("session/cancel")
    })
    .await;
    let messages = wait_transcript(&daemon, |message| {
        message.get("result").is_some_and(|result| {
            result.pointer("/outcome/outcome").and_then(Value::as_str) == Some("cancelled")
        })
    })
    .await;

    assert!(
        messages.iter().any(|message| {
            message["method"] == json!("session/cancel")
                && message["params"]["sessionId"] == json!(acp_id)
        }),
        "session/cancel notification missing: {messages:?}"
    );
    let reply = messages
        .iter()
        .find(|message| {
            message.get("result").is_some_and(|result| {
                result.pointer("/outcome/outcome").and_then(Value::as_str) == Some("cancelled")
            })
        })
        .expect("a cancelled permission reply");
    assert!(
        reply["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("perm-")),
        "the cancelled reply must answer the agent's permission request: {reply:?}"
    );

    // The connection stays usable: the first message was still `initialize`.
    assert_eq!(messages[0]["method"], json!("initialize"));
}
