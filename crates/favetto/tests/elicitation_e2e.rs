//! End-to-end ACP v1 form elicitation tests.
//!
//! The real daemon is spawned against the deterministic fake ACP agent. The
//! tests cover capability advertisement, accepted values, decline/cancel,
//! unsupported schemas and duplicate replies. A unique submitted value is
//! scanned across every emitted event to prove it never leaks into the feed.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use favetto_core::rpc::{method, push, Notification};
use favetto_tui::client::{Client, Transport};
use serde_json::{json, Value};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(25);

struct Daemon {
    root: PathBuf,
    socket: PathBuf,
    child: Child,
}

impl Daemon {
    fn spawn(agent_env: impl FnOnce(&Path) -> String) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "favetto-elicitation-{}-{nanos}",
            std::process::id()
        ));
        for dir in ["data", "tasks", "work"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let agent_env = agent_env(&root);
        let agent = env!("CARGO_BIN_EXE_fake-acp-agent");
        let config = format!(
            "[agent]\ndefault = \"form\"\n\n[agents.form]\ncommand = {agent:?}\nargs = []\n\
             [agents.form.env]\n{agent_env}\n"
        );
        std::fs::write(root.join("config.toml"), config).unwrap();
        let socket = root.join("favetto.sock");
        let child = spawn_child(&root);
        Self {
            root,
            socket,
            child,
        }
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    fn record_path(&self) -> PathBuf {
        self.root.join("elicitation.json")
    }

    fn init_path(&self) -> PathBuf {
        self.root.join("initialize.json")
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.root.join("daemon.log")).unwrap_or_default()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn spawn_child(root: &Path) -> Child {
    let log = std::fs::File::create(root.join("daemon.log")).unwrap();
    Command::new(env!("CARGO_BIN_EXE_favetto"))
        .arg("daemon")
        .arg("--config")
        .arg(root.join("config.toml"))
        .arg("--data-dir")
        .arg(root.join("data"))
        .arg("--tasks-dir")
        .arg(root.join("tasks"))
        .arg("--socket")
        .arg(root.join("favetto.sock"))
        .arg("--listen")
        .arg("127.0.0.1:0")
        .current_dir(root)
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .env_remove("FAVETTO_CONFIG")
        .env_remove("FAVETTO_DATA_DIR")
        .spawn()
        .expect("spawn favetto daemon")
}

async fn wait_for_socket(path: &Path) {
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        match tokio::net::UnixStream::connect(path).await {
            Ok(_) => return,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(POLL).await;
            }
            Err(error) => panic!("daemon socket never became ready: {error}"),
        }
    }
}

fn result(response: favetto_core::rpc::Response, method: &str) -> Value {
    assert!(
        response.error.is_none(),
        "{method} failed: {:?}",
        response.error
    );
    response.result.unwrap_or(Value::Null)
}

async fn start(client: &Client, daemon: &Daemon) -> String {
    let started = result(
        client
            .request(
                method::AGENTS_START,
                json!({
                    "agent": "form",
                    "cwd": daemon.work(),
                    "prompt": "start the form test"
                }),
            )
            .await
            .unwrap(),
        method::AGENTS_START,
    );
    started["session"]["id"]
        .as_str()
        .expect("session id")
        .to_string()
}

async fn next_event(
    rx: &mut tokio::sync::broadcast::Receiver<Notification>,
    session_id: &str,
    kind: &str,
) -> Value {
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        match tokio::time::timeout(POLL, rx.recv()).await {
            Ok(Ok(notification)) if notification.method == push::AGENT_EVENT => {
                let params = &notification.params;
                if params["session_id"].as_str() != Some(session_id) {
                    continue;
                }
                let event = &params["event"];
                if event["event"].as_str() == Some(kind) {
                    return event.clone();
                }
            }
            Ok(Ok(_)) | Ok(Err(_)) => {}
            Err(_) if tokio::time::Instant::now() < deadline => {}
            Err(_) => panic!("no '{kind}' event for session '{session_id}'"),
        }
    }
}

async fn wait_for_record(daemon: &Daemon) -> Value {
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Ok(text) = std::fs::read_to_string(daemon.record_path()) {
            if let Ok(value) = serde_json::from_str::<Value>(&text) {
                return value;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("the fake agent never recorded an elicitation response");
        }
        tokio::time::sleep(POLL).await;
    }
}

fn accept_reply(value: &str) -> Value {
    json!({
        "session_id": "",
        "request_id": "",
        "reply": {
            "action": "accept",
            "values": {
                "name": { "type": "string", "value": value },
                "color": { "type": "string", "value": "blue" },
                "count": { "type": "integer", "value": 3 },
                "ratio": { "type": "number", "value": 0.5 },
                "enabled": { "type": "boolean", "value": true }
            }
        }
    })
}

fn schema() -> String {
    json!({
        "type": "object",
        "title": "Deployment",
        "description": "Review before sending",
        "properties": {
            "name": { "type": "string", "title": "Name", "minLength": 2, "maxLength": 40 },
            "color": { "type": "string", "enum": ["red", "blue"] },
            "count": { "type": "integer", "minimum": 1, "maximum": 9 },
            "ratio": { "type": "number", "minimum": 0.0, "maximum": 1.0 },
            "enabled": { "type": "boolean", "default": false }
        },
        "required": ["name"]
    })
    .to_string()
}

#[tokio::test]
async fn daemon_advertises_form_elicitation_and_not_url() {
    let daemon = Daemon::spawn(|root| {
        format!(
            "FAKE_ACP_INIT_RECORD = {:?}\n",
            root.join("initialize.json")
        )
    });
    wait_for_socket(&daemon.socket).await;
    let client = Client::connect(Transport::Unix(daemon.socket.clone()))
        .await
        .unwrap();
    let _ = start(&client, &daemon).await;

    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    let capabilities = loop {
        if let Ok(text) = std::fs::read_to_string(daemon.init_path()) {
            break serde_json::from_str::<Value>(&text).unwrap();
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("the fake agent never recorded initialize capabilities");
        }
        tokio::time::sleep(POLL).await;
    };
    assert_eq!(capabilities["elicitation"]["form"], json!({}));
    assert!(
        capabilities["elicitation"].get("url").is_none(),
        "URL mode must stay unadvertised: {capabilities}"
    );
}

#[tokio::test]
async fn form_accept_round_trips_values_without_leaking_them() {
    let unique = "unique-form-value-never-logged";
    let daemon = Daemon::spawn(|root| {
        format!(
            "FAKE_ACP_ELICITATION_SCHEMA = {:?}\n\
             FAKE_ACP_ELICITATION_MESSAGE = \"Please fill in the deployment form\"\n\
             FAKE_ACP_ELICITATION_RECORD = {:?}\n",
            schema(),
            root.join("elicitation.json")
        )
    });
    wait_for_socket(&daemon.socket).await;
    let client = Client::connect(Transport::Unix(daemon.socket.clone()))
        .await
        .unwrap();
    let mut pushes = client.subscribe();
    let local = start(&client, &daemon).await;

    let event = next_event(&mut pushes, &local, "elicitation_requested").await;
    let request = &event["request"];
    assert_eq!(request["agent"], "form");
    assert_eq!(request["message"], "Please fill in the deployment form");
    assert_eq!(request["form"]["title"], "Deployment");
    assert_eq!(request["form"]["fields"].as_array().unwrap().len(), 5);
    let request_id = request["id"].as_str().expect("request id").to_string();

    let mut params = accept_reply(unique);
    params["session_id"] = json!(local);
    params["request_id"] = json!(request_id);
    result(
        client
            .request(method::AGENTS_REPLY_ELICITATION, params)
            .await
            .unwrap(),
        method::AGENTS_REPLY_ELICITATION,
    );

    let resolved = next_event(&mut pushes, &local, "elicitation_resolved").await;
    assert_eq!(resolved["outcome"], "accepted");

    let record = wait_for_record(&daemon).await;
    let response = &record["responses"][0];
    assert_eq!(response["action"], "accept");
    assert_eq!(response["content"]["name"], unique);
    assert_eq!(response["content"]["color"], "blue");
    assert_eq!(response["content"]["count"], 3);
    assert_eq!(response["content"]["enabled"], true);

    // Drain every remaining push and assert the submitted value is absent.
    while let Ok(notification) = pushes.try_recv() {
        assert!(
            !notification.params.to_string().contains(unique),
            "a submitted value leaked into an agent event: {notification:?}"
        );
    }
    assert!(
        !daemon.log().contains(unique),
        "a submitted value leaked into the daemon log"
    );
}

#[tokio::test]
async fn decline_and_cancel_are_distinct() {
    for (action, outcome) in [("decline", "declined"), ("cancel", "cancelled")] {
        let daemon = Daemon::spawn(|root| {
            format!(
                "FAKE_ACP_ELICITATION_SCHEMA = {:?}\n\
                 FAKE_ACP_ELICITATION_RECORD = {:?}\n",
                schema(),
                root.join("elicitation.json")
            )
        });
        wait_for_socket(&daemon.socket).await;
        let client = Client::connect(Transport::Unix(daemon.socket.clone()))
            .await
            .unwrap();
        let mut pushes = client.subscribe();
        let local = start(&client, &daemon).await;
        let event = next_event(&mut pushes, &local, "elicitation_requested").await;
        let request_id = event["request"]["id"].as_str().unwrap().to_string();

        result(
            client
                .request(
                    method::AGENTS_REPLY_ELICITATION,
                    json!({
                        "session_id": local,
                        "request_id": request_id,
                        "reply": { "action": action }
                    }),
                )
                .await
                .unwrap(),
            method::AGENTS_REPLY_ELICITATION,
        );
        let resolved = next_event(&mut pushes, &local, "elicitation_resolved").await;
        assert_eq!(resolved["outcome"], outcome);

        let record = wait_for_record(&daemon).await;
        assert_eq!(record["responses"][0]["action"], action);
    }
}

#[tokio::test]
async fn unsupported_schema_fails_explicitly_and_never_becomes_permission() {
    let daemon = Daemon::spawn(|root| {
        format!(
            "FAKE_ACP_ELICITATION_SCHEMA = {:?}\n\
             FAKE_ACP_ELICITATION_RECORD = {:?}\n",
            json!({
                "type": "object",
                "properties": {
                    "tags": { "type": "array", "items": { "type": "string", "enum": ["a", "b"] } }
                }
            })
            .to_string(),
            root.join("elicitation.json")
        )
    });
    wait_for_socket(&daemon.socket).await;
    let client = Client::connect(Transport::Unix(daemon.socket.clone()))
        .await
        .unwrap();
    let mut pushes = client.subscribe();
    let local = start(&client, &daemon).await;

    let record = wait_for_record(&daemon).await;
    assert_eq!(record["responses"][0]["error"]["code"], -32602);
    let error = &record["responses"][0]["error"];
    let reason = format!("{} {}", error["message"], error["data"]);
    assert!(
        reason.contains("multi-select") || reason.contains("array"),
        "the fake agent must receive the explicit unsupported reason: {reason}"
    );

    // No form event and no permission request was produced for this turn.
    while let Ok(notification) = pushes.try_recv() {
        if notification.method != push::AGENT_EVENT {
            continue;
        }
        let params = &notification.params;
        if params["session_id"].as_str() != Some(local.as_str()) {
            continue;
        }
        assert_ne!(
            params["event"]["event"].as_str(),
            Some("elicitation_requested"),
            "an unsupported schema must not produce a form request"
        );
    }
}

#[tokio::test]
async fn duplicate_reply_is_rejected_and_reaches_the_agent_once() {
    let daemon = Daemon::spawn(|root| {
        format!(
            "FAKE_ACP_ELICITATION_SCHEMA = {:?}\n\
             FAKE_ACP_ELICITATION_RECORD = {:?}\n",
            schema(),
            root.join("elicitation.json")
        )
    });
    wait_for_socket(&daemon.socket).await;
    let client = Client::connect(Transport::Unix(daemon.socket.clone()))
        .await
        .unwrap();
    let mut pushes = client.subscribe();
    let local = start(&client, &daemon).await;
    let event = next_event(&mut pushes, &local, "elicitation_requested").await;
    let request_id = event["request"]["id"].as_str().unwrap().to_string();

    let mut params = accept_reply("duplicate-check");
    params["session_id"] = json!(local);
    params["request_id"] = json!(request_id);
    result(
        client
            .request(method::AGENTS_REPLY_ELICITATION, params.clone())
            .await
            .unwrap(),
        method::AGENTS_REPLY_ELICITATION,
    );
    let resolved = next_event(&mut pushes, &local, "elicitation_resolved").await;
    assert_eq!(resolved["outcome"], "accepted");

    let duplicate = client
        .request(method::AGENTS_REPLY_ELICITATION, params)
        .await
        .unwrap();
    let error = duplicate.error.expect("a duplicate reply must fail");
    assert!(
        error.message.contains("no pending ACP elicitation"),
        "{error:?}"
    );

    let record = wait_for_record(&daemon).await;
    assert_eq!(record["count"], 1, "only one response reached the agent");
}
