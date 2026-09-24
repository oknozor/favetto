//! End-to-end test for the favetto MCP supervisor server (issue #191).
//!
//! Spawns the real `favetto` daemon against an isolated temp dir and a slow fake
//! echo agent, then drives a two-step workflow over the MCP server. The server
//! runs **in-process** over a `tokio::io::duplex`, exactly mirroring how
//! `supervisor_e2e.rs` drives the `favetto-tui` supervisor engine in-process
//! while the daemon stays a real subprocess.
//!
//! The stdio framing is exercised for real: every request is written as one JSON
//! line and every response read back as one JSON line.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

use favetto_tui::client::{Client, Transport};

/// How long the daemon may take to start accepting connections.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a single workflow step may take to reach a terminal state.
const STEP_TIMEOUT: Duration = Duration::from_secs(60);

/// An isolated daemon plus its scratch workspace; dropping the harness kills the
/// child and removes everything it wrote.
struct Daemon {
    root: PathBuf,
    socket: PathBuf,
    child: Child,
}

impl Daemon {
    /// Create a scratch workspace with a slow fake echo agent and the two
    /// supervisor demo tasks, then spawn `favetto daemon` against it.
    fn spawn() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("favetto-mcp-e2e-{}-{nanos}", std::process::id()));
        let data_dir = root.join("data");
        let tasks_dir = root.join("tasks");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&tasks_dir).unwrap();

        let agent = root.join("echo-agent.sh");
        std::fs::write(
            &agent,
            "#!/bin/sh\nsleep 0.4\nprintf 'echo-agent: %s\\n' \"$1\"\nprintf 'ECHO_AGENT_DONE\\n'\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        for name in ["supervisor_plan", "supervisor_implement"] {
            std::fs::write(
                tasks_dir.join(format!("{name}.md")),
                format!("agent = \"echo\"\n---\n{name}\n"),
            )
            .unwrap();
        }

        let config_path = root.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\ndefault = \"echo\"\n\n\
                 [agents.echo]\ntype = \"configurable\"\ncommand = {:?}\n\
                 headless_args = [\"{{prompt}}\"]\n\n\
                 [executor]\ndetect_awaiting_input = false\n",
                agent.to_string_lossy()
            ),
        )
        .unwrap();

        let socket = root.join("favetto.sock");
        let listen = format!("127.0.0.1:{}", free_port());
        let log_path = root.join("daemon.log");
        let log = std::fs::File::create(&log_path).unwrap();

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
            .arg(&listen)
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
            child,
        }
    }

    fn socket(&self) -> &Path {
        &self.socket
    }

    /// The captured daemon log, for assertion failure messages.
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

/// A free loopback port. The listener is dropped immediately, so a small race
/// remains; acceptable for a test.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// Block until the Unix socket accepts connections, or panic with the log.
async fn wait_for_socket(path: &Path, daemon: &Daemon) {
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        match tokio::net::UnixStream::connect(path).await {
            Ok(_) => return,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => panic!(
                "daemon socket {} never became ready: {e}\n--- daemon log ---\n{}",
                path.display(),
                daemon.log()
            ),
        }
    }
}

/// A line-delimited MCP client over one half of a duplex.
struct McpClient {
    reader: BufReader<ReadHalf<DuplexStream>>,
    writer: WriteHalf<DuplexStream>,
    next_id: i64,
}

impl McpClient {
    fn new(stream: DuplexStream) -> Self {
        let (read, write) = tokio::io::split(stream);
        Self {
            reader: BufReader::new(read),
            writer: write,
            next_id: 1,
        }
    }

    async fn send(&mut self, message: &Value) {
        let mut line = serde_json::to_string(message).unwrap();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    /// Send a notification (no `id`); the server must not reply.
    async fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await;
    }

    /// Send a request and read the single response line, asserting the id echo.
    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await;

        let mut line = String::new();
        self.reader.read_line(&mut line).await.unwrap();
        assert!(!line.trim().is_empty(), "no response for `{method}`");
        let response: Value = serde_json::from_str(line.trim())
            .unwrap_or_else(|e| panic!("bad response for `{method}`: {e}: {line}"));
        assert_eq!(
            response["id"], id,
            "response id mismatch for `{method}`: {response}"
        );
        response
    }

    /// Call a tool and decode the JSON payload of its text content block.
    async fn tool(&mut self, name: &str, arguments: Value) -> Value {
        let response = self
            .call(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await;
        let result = &response["result"];
        let text = result["content"][0]["text"].as_str().unwrap_or_default();
        assert_ne!(result["isError"], true, "tool `{name}` failed: {text}");
        serde_json::from_str(text)
            .unwrap_or_else(|e| panic!("tool `{name}` returned non-JSON: {e}: {text}"))
    }
}

/// Inspect a root until `task_name` reaches `status`, returning the last view.
async fn wait_for_status(
    mcp: &mut McpClient,
    root_id: &str,
    task_name: &str,
    status: &str,
) -> Value {
    let deadline = tokio::time::Instant::now() + STEP_TIMEOUT;
    loop {
        let view = mcp
            .tool("favetto_inspect", json!({ "root_id": root_id }))
            .await;
        let reached = view["tasks"]
            .as_array()
            .and_then(|tasks| tasks.iter().find(|t| t["name"] == task_name))
            .is_some_and(|task| task["status"] == status);
        if reached {
            return view;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "task `{task_name}` never reached `{status}`: {view}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn mcp_supervisor_drives_a_two_step_workflow() {
    let daemon = Daemon::spawn();
    wait_for_socket(daemon.socket(), &daemon).await;

    let client = Client::connect(Transport::Unix(daemon.socket().to_path_buf()))
        .await
        .expect("connect to the daemon");

    // Serve MCP in-process over a duplex; the daemon client lives in the server.
    let (server_side, test_side) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_side);
    tokio::spawn(async move {
        let _ = favetto_mcp::server::serve(server_read, server_write, client).await;
    });
    let mut mcp = McpClient::new(test_side);

    // Handshake.
    let init = mcp
        .call(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "mcp-e2e", "version": "0" },
            }),
        )
        .await;
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    mcp.notify("notifications/initialized", json!({})).await;

    // The vocabulary is closed at the 13 supervisor tools, and the raw PTY /
    // administration methods are deliberately absent.
    let tools = mcp.call("tools/list", json!({})).await;
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 13, "unexpected tool count: {names:?}");
    for expected in [
        "favetto_inspect",
        "favetto_start_task",
        "favetto_spawn",
        "favetto_cancel_workflow",
        "favetto_cancel_task",
        "favetto_retry_task",
        "favetto_get_task",
        "favetto_list_tasks",
        "favetto_agents_list",
        "favetto_agents_reply",
        "favetto_events_tail",
        "favetto_wait",
    ] {
        assert!(names.contains(&expected), "missing tool `{expected}`");
    }
    for excluded in [
        "agents.input",
        "agents.start",
        "agents.resize",
        "tasks.start_oneshot",
        "catalog.add",
        "catalog.update",
        "schedules.upsert",
        "hooks.upsert",
    ] {
        assert!(!names.contains(&excluded), "exposed tool `{excluded}`");
    }

    // Step 1: start the plan as its own root.
    let task = mcp
        .tool("favetto_start_task", json!({ "name": "supervisor_plan" }))
        .await;
    let root_id = task["id"].as_str().expect("started task id").to_string();

    // The event-cursor wait observes the first terminal event.
    let waited = mcp
        .tool(
            "favetto_wait",
            json!({ "until": ["task_finished"], "timeout_ms": 15_000 }),
        )
        .await;
    assert_eq!(waited["timed_out"], false, "wait saw no event: {waited}");
    assert!(
        waited["events"]
            .as_array()
            .is_some_and(|events| !events.is_empty()),
        "wait returned no events: {waited}"
    );

    let view = wait_for_status(&mut mcp, &root_id, "supervisor_plan", "succeeded").await;
    assert_eq!(view["state"], "succeeded", "{view}");

    // Step 2: spawn the implementation into the same root, deduped.
    let spawned = mcp
        .tool(
            "favetto_spawn",
            json!({
                "root_id": root_id,
                "name": "supervisor_implement",
                "dedupe_key": "mcp-e2e:implement",
            }),
        )
        .await;
    let implement_id = spawned["id"].as_str().expect("spawned task id").to_string();

    let view = wait_for_status(&mut mcp, &root_id, "supervisor_implement", "succeeded").await;
    assert_eq!(view["state"], "succeeded", "{view}");

    // Both steps share the root; the supervisor (not a catalog edge) created the
    // second, so its `root_id` is the root and not the plan task.
    let row = mcp
        .tool("favetto_get_task", json!({ "id": implement_id }))
        .await;
    assert_eq!(row["root_id"], root_id, "{row}");

    // The workflow resource reads a `succeeded` snapshot.
    let read = mcp
        .call(
            "resources/read",
            json!({ "uri": format!("favetto://workflow/{root_id}") }),
        )
        .await;
    let text = read["result"]["contents"][0]["text"]
        .as_str()
        .expect("resource text");
    let snapshot: Value = serde_json::from_str(text).expect("resource JSON");
    assert_eq!(snapshot["state"], "succeeded", "{snapshot}");
    assert!(snapshot["tasks"]
        .as_array()
        .is_some_and(|tasks| tasks.iter().any(|t| t["name"] == "supervisor_plan")));

    // Catalog resource maps to `catalog.list`.
    let catalog = mcp
        .call("resources/read", json!({ "uri": "favetto://catalog" }))
        .await;
    assert!(catalog["result"]["contents"][0]["text"]
        .as_str()
        .is_some_and(|text| text.contains("supervisor_plan")));

    // The prompt carries the closed vocabulary contract.
    let prompt = mcp
        .call(
            "prompts/get",
            json!({ "name": "supervise_workflow", "arguments": { "root_id": root_id } }),
        )
        .await;
    let prompt_text = prompt["result"]["messages"][0]["content"]["text"]
        .as_str()
        .expect("prompt text");
    for action in [
        "inspect",
        "spawn",
        "cancel",
        "retry",
        "wait",
        "request_input",
        "complete",
        "escalate",
    ] {
        assert!(prompt_text.contains(action), "prompt missing `{action}`");
    }
    assert!(prompt_text.contains("EXACTLY ONE decision per cycle"));
}
