//! End-to-end ACP v1 authentication tests.
//!
//! These spawn the real daemon against the deterministic fake ACP server. They
//! cover agent-handled sign-in, terminal-method rejection, auth-required session
//! setup, capability-gated logout, and the guarantee that terminal launch
//! configuration (which can contain secrets) never reaches clients or logs.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use favetto_core::rpc::method;
use favetto_tui::client::{Client, Transport};
use serde_json::json;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(25);

struct Daemon {
    root: PathBuf,
    socket: PathBuf,
    child: Child,
}

impl Daemon {
    fn spawn(agents: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("favetto-acp-auth-{}-{nanos}", std::process::id()));
        let data_dir = root.join("data");
        let tasks_dir = root.join("tasks");
        let work_dir = root.join("work");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::create_dir_all(&work_dir).unwrap();

        let config = format!("[agent]\ndefault = \"login\"\n\n{agents}");
        std::fs::write(root.join("config.toml"), config).unwrap();

        let socket = root.join("favetto.sock");
        let child = spawn_child(&root);
        Self {
            root,
            socket,
            child,
        }
    }

    /// Stop, replace the agent config, and start against the same data dir.
    fn reconfigure(&mut self, agents: &str) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let config = format!("[agent]\ndefault = \"login\"\n\n{agents}");
        std::fs::write(self.root.join("config.toml"), config).unwrap();
        let _ = std::fs::remove_file(&self.socket);
        self.child = spawn_child(&self.root);
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
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

fn agent_block(name: &str, env: &str) -> String {
    let agent = env!("CARGO_BIN_EXE_fake-acp-agent");
    // These tests load session ids created by an earlier agent process (the
    // fake models an agent with server-side persistence).
    format!(
        "[agents.{name}]\ncommand = {agent:?}\nargs = []\n[agents.{name}.env]\n{env}FAKE_ACP_LENIENT_SESSIONS = \"1\"\n"
    )
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

fn result(response: favetto_core::rpc::Response, method: &str) -> serde_json::Value {
    assert!(
        response.error.is_none(),
        "{method} failed: {:?}",
        response.error
    );
    response.result.unwrap_or(serde_json::Value::Null)
}

async fn agent_session(client: &Client, session_id: &str) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        let list = result(
            client
                .request(method::AGENTS_LIST, json!({}))
                .await
                .unwrap(),
            method::AGENTS_LIST,
        );
        let found = list.as_array().and_then(|agents| {
            agents
                .iter()
                .flat_map(|agent| agent["sessions"].as_array().into_iter().flatten())
                .find(|session| session["id"].as_str() == Some(session_id))
                .cloned()
        });
        if let Some(session) = found {
            let status = session["auth"]["status"].as_str();
            let settled = matches!(status, Some("required" | "failed"))
                || (session["session_id"].is_string() && status != Some("required"));
            if settled {
                return session;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("session '{session_id}' never reached the expected auth state");
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn wait_for_ready(client: &Client, session_id: &str) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        let session = agent_session(client, session_id).await;
        if session["session_id"].is_string()
            && session["auth"]["status"].as_str() != Some("required")
        {
            return session;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("session '{session_id}' was never set up after sign-in");
        }
        tokio::time::sleep(POLL).await;
    }
}

#[tokio::test]
async fn agent_handled_sign_in_is_advertised_and_retries_session_setup() {
    let secret = "terminal-env-secret-do-not-expose";
    let methods = json!([
        {
            "id": "fake-login",
            "name": "Fake sign in",
            "description": "Agent handles this method"
        },
        {
            "type": "terminal",
            "id": "terminal-login",
            "name": "Terminal sign in",
            "args": ["auth", "--interactive"],
            "env": { "TERMINAL_AUTH_SECRET": secret }
        }
    ])
    .to_string();
    let daemon = Daemon::spawn(&agent_block(
        "login",
        &format!(
            "FAKE_ACP_AUTH_REQUIRED = \"1\"\n\
             FAKE_ACP_AUTH_METHODS = {methods:?}\n\
             FAKE_ACP_AUTH_LOGOUT = \"1\"\n"
        ),
    ));
    wait_for_socket(&daemon.socket).await;
    let client = Client::connect(Transport::Unix(daemon.socket.clone()))
        .await
        .unwrap();

    let started = result(
        client
            .request(
                method::AGENTS_START,
                json!({ "agent": "login", "cwd": daemon.work() }),
            )
            .await
            .unwrap(),
        method::AGENTS_START,
    );
    let local = started["session"]["id"].as_str().expect("session id");

    let required = agent_session(&client, local).await;
    let auth = &required["auth"];
    assert_eq!(auth["status"], "required");
    assert_eq!(auth["logout_supported"], true);
    let methods = auth["methods"].as_array().expect("methods");
    let agent_method = methods
        .iter()
        .find(|method| method["id"] == "fake-login")
        .expect("agent method");
    assert_eq!(agent_method["supported"], true);
    assert_eq!(agent_method["kind"], "agent");
    let terminal = methods
        .iter()
        .find(|method| method["id"] == "terminal-login")
        .expect("terminal method");
    assert_eq!(terminal["supported"], false);
    assert_eq!(terminal["kind"], "terminal");
    assert!(
        !auth.to_string().contains(secret),
        "terminal launch env leaked into client state: {auth}"
    );

    // Terminal methods are preserved for display but explicitly rejected by the
    // daemon, which cannot bridge an interactive login for a remote client.
    let rejected = client
        .request(
            method::AGENTS_AUTHENTICATE,
            json!({ "session_id": local, "method_id": "terminal-login" }),
        )
        .await
        .unwrap();
    let error = rejected.error.expect("terminal auth must fail");
    assert!(
        error
            .message
            .contains("terminal authentication is not supported"),
        "{error:?}"
    );

    let authenticated = result(
        client
            .request(
                method::AGENTS_AUTHENTICATE,
                json!({ "session_id": local, "method_id": "fake-login" }),
            )
            .await
            .unwrap(),
        method::AGENTS_AUTHENTICATE,
    );
    assert_eq!(authenticated["authenticated"], true);

    let ready = wait_for_ready(&client, local).await;
    assert_eq!(ready["running"], true);
    assert!(ready["session_id"].is_string());

    // Sign-out is advertised, so the same session can complete it.
    let logged_out = result(
        client
            .request(method::AGENTS_LOGOUT, json!({ "session_id": local }))
            .await
            .unwrap(),
        method::AGENTS_LOGOUT,
    );
    assert_eq!(logged_out["logged_out"], true);

    let log = daemon.log();
    assert!(
        !log.contains(secret),
        "terminal launch env leaked into the daemon log"
    );
}

#[tokio::test]
async fn auth_failure_is_retryable_and_logout_requires_the_capability() {
    let daemon = Daemon::spawn(&format!(
        "{}{}",
        agent_block(
            "login",
            "FAKE_ACP_AUTH_REQUIRED = \"1\"\n\
             FAKE_ACP_AUTH_METHODS = \"[{\\\"id\\\":\\\"fake-login\\\",\\\"name\\\":\\\"Fake sign in\\\"}]\"\n\
             FAKE_ACP_AUTH_RESULT = \"error\"\n",
        ),
        agent_block(
            "plain",
            "FAKE_ACP_AUTH_METHODS = \"[{\\\"id\\\":\\\"fake-login\\\",\\\"name\\\":\\\"Fake sign in\\\"}]\"\n",
        ),
    ));
    wait_for_socket(&daemon.socket).await;
    let client = Client::connect(Transport::Unix(daemon.socket.clone()))
        .await
        .unwrap();

    let started = result(
        client
            .request(
                method::AGENTS_START,
                json!({ "agent": "login", "cwd": daemon.work() }),
            )
            .await
            .unwrap(),
        method::AGENTS_START,
    );
    let local = started["session"]["id"].as_str().expect("session id");
    let required = agent_session(&client, local).await;
    assert_eq!(required["auth"]["status"], "required");

    let failed = client
        .request(
            method::AGENTS_AUTHENTICATE,
            json!({ "session_id": local, "method_id": "fake-login" }),
        )
        .await
        .unwrap();
    let error = failed.error.expect("a rejected sign-in must fail");
    assert!(error.message.contains("sign-in failed"), "{error:?}");

    // A failed sign-in leaves a retryable session, not a dead one.
    let after_failure = agent_session(&client, local).await;
    assert_eq!(after_failure["auth"]["status"], "failed");
    assert_eq!(after_failure["running"], true);
    assert!(after_failure["session_id"].is_null());

    // Logout was not advertised for this agent.
    let no_logout = client
        .request(method::AGENTS_LOGOUT, json!({ "session_id": local }))
        .await
        .unwrap();
    let error = no_logout.error.expect("logout must be capability-gated");
    assert!(
        error.message.contains("did not advertise ACP logout"),
        "{error:?}"
    );

    // An agent that advertises methods but not logout cannot sign out either,
    // even when its session is ready.
    let plain = result(
        client
            .request(
                method::AGENTS_START,
                json!({ "agent": "plain", "cwd": daemon.work() }),
            )
            .await
            .unwrap(),
        method::AGENTS_START,
    );
    let plain_id = plain["session"]["id"].as_str().expect("session id");
    wait_for_ready(&client, plain_id).await;
    let no_logout = client
        .request(method::AGENTS_LOGOUT, json!({ "session_id": plain_id }))
        .await
        .unwrap();
    assert!(no_logout.error.is_some(), "logout must stay gated");
}

#[tokio::test]
async fn a_load_that_needs_sign_in_returns_a_retryable_session() {
    let mut daemon = Daemon::spawn(&agent_block("login", ""));
    wait_for_socket(&daemon.socket).await;
    let client = Client::connect(Transport::Unix(daemon.socket.clone()))
        .await
        .unwrap();

    let started = result(
        client
            .request(
                method::AGENTS_START,
                json!({ "agent": "login", "cwd": daemon.work() }),
            )
            .await
            .unwrap(),
        method::AGENTS_START,
    );
    let local = started["session"]["id"]
        .as_str()
        .expect("session id")
        .to_string();
    wait_for_ready(&client, &local).await;
    result(
        client
            .request(method::AGENTS_CLOSE, json!({ "session_id": local }))
            .await
            .unwrap(),
        method::AGENTS_CLOSE,
    );
    drop(client);

    daemon.reconfigure(&agent_block(
        "login",
        "FAKE_ACP_AUTH_REQUIRED = \"1\"\n\
         FAKE_ACP_AUTH_METHODS = \"[{\\\"id\\\":\\\"fake-login\\\",\\\"name\\\":\\\"Fake sign in\\\"}]\"\n",
    ));
    wait_for_socket(&daemon.socket).await;
    let client = Client::connect(Transport::Unix(daemon.socket.clone()))
        .await
        .unwrap();

    // A load paused on auth-required is a usable, retryable session snapshot,
    // not a failed restore.
    let loaded = result(
        client
            .request(method::AGENTS_LOAD, json!({ "session_id": local }))
            .await
            .unwrap(),
        method::AGENTS_LOAD,
    );
    assert_eq!(loaded["session"]["auth"]["status"], "required");

    result(
        client
            .request(
                method::AGENTS_AUTHENTICATE,
                json!({ "session_id": local, "method_id": "fake-login" }),
            )
            .await
            .unwrap(),
        method::AGENTS_AUTHENTICATE,
    );
    let ready = wait_for_ready(&client, &local).await;
    assert_eq!(ready["running"], true);
    assert!(ready["session_id"].is_string());
}

#[tokio::test]
async fn an_agent_without_auth_methods_has_no_auth_state() {
    let daemon = Daemon::spawn(&agent_block("plain", ""));
    wait_for_socket(&daemon.socket).await;
    let client = Client::connect(Transport::Unix(daemon.socket.clone()))
        .await
        .unwrap();

    let started = result(
        client
            .request(
                method::AGENTS_START,
                json!({ "agent": "plain", "cwd": daemon.work() }),
            )
            .await
            .unwrap(),
        method::AGENTS_START,
    );
    let local = started["session"]["id"].as_str().expect("session id");
    let session = wait_for_ready(&client, local).await;
    assert!(session.get("auth").is_none(), "{session}");

    let rejected = client
        .request(
            method::AGENTS_AUTHENTICATE,
            json!({ "session_id": local, "method_id": "anything" }),
        )
        .await
        .unwrap();
    assert!(rejected.error.is_some());
}
