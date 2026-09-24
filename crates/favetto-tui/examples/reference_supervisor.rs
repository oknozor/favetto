//! Runnable reference external supervisor.
//!
//! A worked example of the [supervisor contract](../../../docs/reference/supervisor-contract.md):
//! a controller **outside the daemon** that observes `workflow.inspect`, decides
//! `wait`/`spawn`, and submits the workflow RPCs. It drives the two catalog tasks
//! in `tasks/examples/supervisor/`:
//!
//! 1. start `examples/supervisor/plan` as its own root;
//! 2. `wait` for it to succeed;
//! 3. `workflow.spawn` `examples/supervisor/implement` into the same root;
//! 4. `wait` for the root to succeed, then `complete`.
//!
//! The sequencing lives here, not in a `needs`/`spawn` header, and the daemon
//! gains no supervisor code: this is an ordinary remote-API client.
//!
//! ```bash
//! # terminal 1
//! favetto daemon --socket /tmp/favetto.sock
//!
//! # terminal 2 (from the repo root, with the examples on the catalog)
//! cargo run -p favetto-tui --example reference_supervisor -- --socket /tmp/favetto.sock
//! ```

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;

use favetto_tui::client::Client;
use favetto_tui::supervisor::{self, Action, Policy};

#[derive(Parser)]
#[command(
    name = "reference_supervisor",
    about = "Drive a two-step favetto workflow from outside the daemon"
)]
struct Args {
    /// Unix socket of a local daemon (default `/tmp/favetto.sock`).
    #[arg(long)]
    socket: Option<PathBuf>,
    /// WebSocket URL of a remote daemon (`ws://…`). Overrides `--socket`.
    #[arg(long)]
    remote: Option<String>,
    /// Bearer token file for remote auth (defaults to `<data_dir>/token`).
    #[arg(long)]
    token_file: Option<PathBuf>,
    /// First catalog task; started as the root.
    #[arg(long, default_value = "examples/supervisor/plan")]
    first: String,
    /// Second catalog task; spawned once the first succeeds.
    #[arg(long, default_value = "examples/supervisor/implement")]
    second: String,
    /// JSON input for the spawned second step (`null` to send none).
    #[arg(long, default_value = "null")]
    input: String,
    /// Dedupe key for the spawned second step.
    #[arg(long, default_value = "examples/supervisor:implement")]
    dedupe_key: String,
    /// Pause between inspections when waiting, in milliseconds.
    #[arg(long, default_value_t = 250)]
    wait_ms: u64,
    /// Safety bound on the number of decision cycles.
    #[arg(long, default_value_t = 240)]
    max_cycles: usize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let input: serde_json::Value =
        serde_json::from_str(&args.input).context("--input must be valid JSON")?;
    let transport = favetto_tui::client::resolve_transport(
        args.remote.clone(),
        args.socket.clone(),
        args.token_file.clone(),
    );
    let client = Client::connect(transport)
        .await
        .context("connect to the favetto daemon")?;

    let policy = Policy {
        first_task: args.first,
        second_task: args.second,
        input,
        dedupe_key: args.dedupe_key,
        wait: Duration::from_millis(args.wait_ms),
        max_cycles: args.max_cycles,
    };

    let root_id = supervisor::start_root(&client, &policy).await?;
    println!("started '{}' as root {root_id}", policy.first_task);

    let outcome = supervisor::run(&client, &policy, root_id).await?;
    for decision in &outcome.decisions {
        println!(
            "{:?}: {} {}",
            decision.action,
            decision.reason,
            if decision.params.is_null() {
                String::new()
            } else {
                format!("{}", decision.params)
            }
        );
    }

    match outcome.terminal.action {
        Action::Complete => Ok(()),
        Action::Escalate => anyhow::bail!("escalated: {}", outcome.terminal.reason),
        other => anyhow::bail!("unexpected terminal decision {other:?}"),
    }
}
