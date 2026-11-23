//! End-to-end tests for the MCP server registry and ACP `mcpServers` forwarding.
//!
//! The real daemon is spawned against the deterministic fake ACP agent, which
//! records every `session/new`, `session/load` and `session/resume` MCP list.
//! The tests cover stdio forwarding (absolute command, args, resolved env), HTTP
//! capability gating, approval gating, explicit selection, secret redaction and
//! replaying the recorded selection on restore.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use favetto_core::rpc::method;
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
    fn spawn() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "favetto-mcp-registry-{}-{nanos}",
            std::process::id()
        ));
        for dir in ["data", "tasks", "work"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("config.toml"), config(&root)).unwrap();
        let socket = root.join("favetto.sock");
        let child = spawn_child(&root);
        Self {
            root,
            socket,
            child,
        }
    }

    fn marker(&self, agent: &str) -> PathBuf {
        self.root.join(format!("mcp-{agent}.jsonl"))
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.root.join("daemon.log")).unwrap_or_default()
    }

    fn restart(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.socket = self.root.join("favetto.sock");
        let _ = std::fs::remove_file(&self.socket);
        self.child = spawn_child(&self.root);
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn config(root: &Path) -> String {
    let agent = env!("CARGO_BIN_EXE_fake-acp-agent");
    let plain = root.join("mcp-plain.jsonl");
    let caps = root.join("mcp-caps.jsonl");
    format!(
        "[agent]\ndefault = \"plain\"\n\n\
         [agents.plain]\ncommand = {agent:?}\nargs = []\n\
         [agents.plain.env]\nFAKE_ACP_MCP_MARKER = {plain:?}\n\n\
         [agents.caps]\ncommand = {agent:?}\nargs = []\n\
         [agents.caps.env]\n\
         FAKE_ACP_MCP_MARKER = {caps:?}\n\
         FAKE_ACP_MCP_HTTP = \"1\"\n\n\
         [[mcp.servers]]\n\
         name = \"seeded\"\n\
         command = \"/bin/sh\"\n\
         args = [\"-c\", \"true\"]\n\
         env = {{ SEED_TOKEN = \"seed-secret\" }}\n\
         env_from = {{ ENV_TOKEN = \"FAVETTO_TEST_MCP_TOKEN\" }}\n"
    )
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
        .env("FAVETTO_TEST_MCP_TOKEN", "env-secret")
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

async fn connect(daemon: &Daemon) -> Client {
    wait_for_socket(&daemon.socket).await;
    Client::connect(Transport::Unix(daemon.socket.clone()))
        .await
        .unwrap()
}

async fn wait_ready(client: &Client, started: &Value) -> Value {
    let id = started["session"]["id"].as_str().expect("session id");
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        let attached = result(
            client
                .request(method::AGENTS_ATTACH, json!({ "session_id": id }))
                .await
                .unwrap(),
            method::AGENTS_ATTACH,
        );
        if attached["session"]["session_id"].is_string() {
            return attached["session"].clone();
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("session '{id}' never finished session setup: {attached}");
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn last_request(daemon: &Daemon, agent: &str) -> Value {
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Ok(raw) = std::fs::read_to_string(daemon.marker(agent)) {
            if let Some(request) = raw
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .next_back()
            {
                return request;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("no MCP marker for agent '{agent}'");
        }
        tokio::time::sleep(POLL).await;
    }
}

fn names(value: &Value) -> Vec<String> {
    let mut names: Vec<String> = value
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn servers(request: &Value) -> Vec<Value> {
    request["servers"].as_array().cloned().unwrap_or_default()
}

async fn add_stdio(client: &Client, name: &str, env: Value, approve: bool) -> Value {
    result(
        client
            .request(
                method::MCP_ADD,
                json!({
                    "server": {
                        "name": name,
                        "transport": "stdio",
                        "command": "/bin/sh",
                        "args": ["-c", "true"],
                        "env": env,
                    },
                    "approve": approve,
                }),
            )
            .await
            .unwrap(),
        method::MCP_ADD,
    )
}

async fn add_http(client: &Client, name: &str) -> Value {
    result(
        client
            .request(
                method::MCP_ADD,
                json!({
                    "server": {
                        "name": name,
                        "transport": "http",
                        "url": "https://example.com/mcp",
                        "headers": [
                            {
                                "name": "Authorization",
                                "source": "literal",
                                "value": "Bearer header-secret"
                            }
                        ]
                    }
                }),
            )
            .await
            .unwrap(),
        method::MCP_ADD,
    )
}

async fn start(client: &Client, daemon: &Daemon, agent: &str, mcp: Option<Vec<&str>>) -> Value {
    let mut params = json!({ "agent": agent, "cwd": daemon.work(), "prompt": "hi" });
    if let Some(mcp) = mcp {
        params["mcp_servers"] = json!(mcp);
    }
    result(
        client.request(method::AGENTS_START, params).await.unwrap(),
        method::AGENTS_START,
    )
}

#[tokio::test]
async fn stdio_servers_are_forwarded_and_secrets_stay_redacted() {
    let daemon = Daemon::spawn();
    let client = connect(&daemon).await;

    // A config seed is operator-authored and pre-approved.
    let listed = result(
        client.request(method::MCP_LIST, json!({})).await.unwrap(),
        method::MCP_LIST,
    );
    let seeded = &listed["servers"][0];
    assert_eq!(seeded["name"], "seeded");
    assert_eq!(seeded["approved"], true);
    assert_eq!(seeded["env"][0]["has_value"], true);
    assert!(seeded.to_string().find("seed-secret").is_none());

    // A literal supplied over RPC is write-only.
    let added = add_stdio(
        &client,
        "files",
        json!([{ "name": "GITHUB_TOKEN", "source": "literal", "value": "rpc-secret" }]),
        true,
    )
    .await;
    assert_eq!(added["approved"], true);
    assert!(added.to_string().find("rpc-secret").is_none());
    assert_eq!(added["env"][0]["value"], Value::Null);

    let started = start(&client, &daemon, "plain", None).await;
    let session = wait_ready(&client, &started).await;
    let request = last_request(&daemon, "plain").await;
    assert_eq!(request["method"], "session/new");
    let forwarded = servers(&request);
    assert_eq!(forwarded.len(), 2, "{request}");
    let files = forwarded
        .iter()
        .find(|server| server["name"] == "files")
        .expect("stdio server forwarded");
    assert_eq!(
        files["command"],
        std::fs::canonicalize("/bin/sh")
            .unwrap()
            .to_string_lossy()
            .into_owned()
    );
    assert_eq!(files["args"], json!(["-c", "true"]));
    assert!(files["env"]
        .as_array()
        .unwrap()
        .iter()
        .any(|env| env["name"] == "GITHUB_TOKEN" && env["value"] == "rpc-secret"));
    let seeded = forwarded
        .iter()
        .find(|server| server["name"] == "seeded")
        .expect("seeded server forwarded");
    assert!(seeded["env"]
        .as_array()
        .unwrap()
        .iter()
        .any(|env| env["name"] == "SEED_TOKEN" && env["value"] == "seed-secret"));
    assert!(seeded["env"]
        .as_array()
        .unwrap()
        .iter()
        .any(|env| env["name"] == "ENV_TOKEN" && env["value"] == "env-secret"));

    // None of the session/registry payloads or the daemon log leak a value.
    let session_json = session.to_string();
    assert!(!session_json.contains("rpc-secret"), "{session_json}");
    assert!(!session_json.contains("seed-secret"), "{session_json}");
    assert!(!daemon.log().contains("rpc-secret"));
}

#[tokio::test]
async fn http_is_gated_on_agent_capabilities_and_warns_when_omitted() {
    let daemon = Daemon::spawn();
    let client = connect(&daemon).await;
    add_http(&client, "remote").await;

    let plain = start(&client, &daemon, "plain", None).await;
    let plain = wait_ready(&client, &plain).await;
    let request = last_request(&daemon, "plain").await;
    assert!(
        servers(&request)
            .iter()
            .all(|server| server["name"] != "remote"),
        "{request}"
    );
    assert_eq!(names(&plain["mcp"]["active"]), vec!["seeded"]);
    let warnings = plain["mcp"]["warnings"].to_string();
    assert!(warnings.contains("remote"), "{warnings}");
    assert!(warnings.contains("http"), "{warnings}");

    let caps = start(&client, &daemon, "caps", None).await;
    let caps = wait_ready(&client, &caps).await;
    let request = last_request(&daemon, "caps").await;
    let remote = servers(&request)
        .into_iter()
        .find(|server| server["name"] == "remote")
        .expect("http server forwarded when supported");
    assert_eq!(remote["url"], "https://example.com/mcp");
    assert!(remote["headers"]
        .as_array()
        .unwrap()
        .iter()
        .any(
            |header| header["name"] == "Authorization" && header["value"] == "Bearer header-secret"
        ));
    assert_eq!(names(&caps["mcp"]["active"]), vec!["remote", "seeded"]);
    assert!(caps["mcp"]["warnings"]
        .as_array()
        .map(Vec::is_empty)
        .unwrap_or(true));
}

#[tokio::test]
async fn unapproved_and_unknown_entries_do_not_block_the_session() {
    let daemon = Daemon::spawn();
    let client = connect(&daemon).await;
    let added = add_stdio(&client, "pending", json!([]), false).await;
    assert_eq!(added["approved"], false);

    let explicit = start(&client, &daemon, "plain", Some(vec!["pending"])).await;
    let explicit = wait_ready(&client, &explicit).await;
    assert!(explicit["mcp"]["active"]
        .as_array()
        .map(Vec::is_empty)
        .unwrap_or(true));
    assert!(explicit["mcp"]["warnings"]
        .to_string()
        .contains("not been approved"));
    // The session still starts and reaches the agent with an empty list.
    assert!(servers(&last_request(&daemon, "plain").await).is_empty());

    // Approving it makes it launchable.
    let approved = result(
        client
            .request(
                method::MCP_APPROVE,
                json!({ "name": "pending", "approved": true }),
            )
            .await
            .unwrap(),
        method::MCP_APPROVE,
    );
    assert_eq!(approved["approved"], true);
    let started = start(&client, &daemon, "plain", Some(vec!["pending"])).await;
    let _ = wait_ready(&client, &started).await;
    assert!(servers(&last_request(&daemon, "plain").await)
        .iter()
        .any(|server| server["name"] == "pending"));

    // Editing the command/args invalidates the fingerprint-bound approval.
    let updated = result(
        client
            .request(
                method::MCP_UPDATE,
                json!({
                    "name": "pending",
                    "server": {
                        "name": "pending",
                        "transport": "stdio",
                        "command": "/bin/sh",
                        "args": ["-c", "false"]
                    }
                }),
            )
            .await
            .unwrap(),
        method::MCP_UPDATE,
    );
    assert_eq!(updated["approved"], false);

    // An unknown *explicit* name is a hard error rather than a silent change.
    let response = client
        .request(
            method::AGENTS_START,
            json!({
                "agent": "plain",
                "cwd": daemon.work(),
                "mcp_servers": ["does-not-exist"]
            }),
        )
        .await
        .unwrap();
    assert!(response.error.is_some(), "{response:?}");
}

#[tokio::test]
async fn registry_and_selection_survive_a_restart_and_replay_on_load() {
    let mut daemon = Daemon::spawn();
    let client = connect(&daemon).await;
    add_stdio(
        &client,
        "files",
        json!([{ "name": "TOKEN", "source": "literal", "value": "durable-secret" }]),
        true,
    )
    .await;

    // Explicit selection excludes the seeded default.
    let started = start(&client, &daemon, "plain", Some(vec!["files"])).await;
    let started = wait_ready(&client, &started).await;
    assert_eq!(names(&started["mcp"]["selected"]), vec!["files"]);
    let forwarded = servers(&last_request(&daemon, "plain").await);
    assert_eq!(forwarded.len(), 1);
    assert_eq!(forwarded[0]["name"], "files");

    let local_id = started["id"].as_str().unwrap().to_string();
    result(
        client
            .request(method::AGENTS_CLOSE, json!({ "session_id": local_id }))
            .await
            .unwrap(),
        method::AGENTS_CLOSE,
    );

    daemon.restart();
    let client = connect(&daemon).await;
    let listed = result(
        client.request(method::MCP_LIST, json!({})).await.unwrap(),
        method::MCP_LIST,
    );
    let registered: Vec<&str> = listed["servers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|server| server["name"].as_str().unwrap())
        .collect();
    assert!(registered.contains(&"files"), "{listed}");
    // The literal value survived the restart but is still not returned.
    assert!(listed.to_string().find("durable-secret").is_none());

    let loaded = result(
        client
            .request(method::AGENTS_LOAD, json!({ "session_id": local_id }))
            .await
            .unwrap(),
        method::AGENTS_LOAD,
    );
    assert_eq!(names(&loaded["session"]["mcp"]["selected"]), vec!["files"]);
    let request = last_request(&daemon, "plain").await;
    assert_eq!(request["method"], "session/load");
    let forwarded = servers(&request);
    assert_eq!(forwarded.len(), 1);
    assert_eq!(forwarded[0]["name"], "files");
    assert!(forwarded[0]["env"]
        .as_array()
        .unwrap()
        .iter()
        .any(|env| env["name"] == "TOKEN" && env["value"] == "durable-secret"));
}
