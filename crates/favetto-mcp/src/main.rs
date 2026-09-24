//! `favetto-mcp` — an MCP (Model Context Protocol) supervisor server for the
//! favetto workflow control plane, speaking JSON-RPC 2.0 over stdio.
//!
//! It connects to a running favetto daemon as an ordinary remote-API client and
//! exposes the closed supervisor vocabulary as MCP tools. Logs go to stderr;
//! stdout is the protocol channel.

use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use favetto_mcp::server;
use favetto_tui::client::Client;

#[derive(Parser)]
#[command(
    name = "favetto-mcp",
    about = "Expose a favetto daemon to an MCP client as a workflow supervisor"
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
    /// Tracing filter (stderr only), e.g. `info` or `favetto_mcp=debug`.
    #[arg(long, default_value = "info")]
    log: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let filter = EnvFilter::try_new(&args.log).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    let transport =
        favetto_tui::client::resolve_transport(args.remote, args.socket, args.token_file);
    let client = Client::connect(transport)
        .await
        .context("connect to the favetto daemon")?;
    tracing::info!("connected; serving MCP over stdio");

    server::serve(tokio::io::stdin(), tokio::io::stdout(), client).await
}
