//! `favetto` — LLM-driven agent orchestrator (daemon + remote TUI).

mod agents;
mod cli;
mod client;
mod config;
mod daemon;
mod db;
mod event_bus;
mod executor;
mod hooks;
mod metrics;
mod notify;
mod pair;
mod scheduler;
mod server;
mod state;
mod tasks;
mod transport;
mod tui;
mod webhooks;

use clap::Parser;

use crate::cli::Command;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "favetto=info".into()),
        )
        .init();

    let cli = cli::Cli::parse();

    match cli.command {
        Command::Daemon(args) => daemon::run(args).await,
        Command::Tui(args) => tui::run(args).await,
        Command::Pair(args) => pair(args).await,
        Command::TokenRotate(args) => token_rotate(args),
        Command::InternalAgentExec { argv } => agent_exec(argv),
    }
}

/// `favetto __agent-exec -- <cmd> <args…>`: run an agent as a child of a small
/// supervisor in its own process group. The supervisor dies (and kills the whole
/// group) when the daemon dies, even on `SIGKILL`, via `PR_SET_PDEATHSIG`.
fn agent_exec(argv: Vec<String>) -> anyhow::Result<()> {
    let argv: Vec<String> = argv.into_iter().skip_while(|a| a == "--").collect();
    let (prog, args) = argv
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("__agent-exec: no command given"))?;

    #[cfg(unix)]
    {
        extern "C" fn on_death(_sig: libc::c_int) {
            // Kill the whole process group: supervisor + agent + descendants.
            // SAFETY: `kill` is async-signal-safe.
            unsafe { libc::kill(0, libc::SIGKILL) };
        }

        // Own process group *before* forking, so the child joins it and the group
        // kill above never reaches the daemon or its other children. (No-op when
        // already a session leader, as under the PTY spawner.)
        // SAFETY: setpgid is safe here; errors (e.g. already a leader) are ignored.
        unsafe {
            libc::setpgid(0, 0);
        }

        // SAFETY: standard fork/exec/waitpid dance; no pointers outlive the call.
        let pid = unsafe { libc::fork() };
        match pid {
            -1 => anyhow::bail!("__agent-exec: fork: {}", std::io::Error::last_os_error()),
            0 => {
                // Child: exec the agent, staying in the supervisor's process group
                // so the group kill reaches it and its descendants.
                use std::os::unix::process::CommandExt;
                let err = std::process::Command::new(prog).args(args).exec();
                eprintln!("__agent-exec: exec {prog}: {err}");
                unsafe { libc::_exit(127) };
            }
            pid => {
                // Supervisor: die (and take the group with us) when the daemon does.
                unsafe {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                    if libc::getppid() == 1 {
                        libc::kill(pid, libc::SIGKILL);
                        libc::_exit(0);
                    }
                    libc::signal(libc::SIGTERM, on_death as *const () as libc::sighandler_t);
                    libc::signal(libc::SIGINT, on_death as *const () as libc::sighandler_t);
                    libc::signal(libc::SIGHUP, on_death as *const () as libc::sighandler_t);
                }
                let mut status: libc::c_int = 0;
                unsafe { libc::waitpid(pid, &mut status, 0) };
                let code = if libc::WIFEXITED(status) {
                    libc::WEXITSTATUS(status)
                } else {
                    1
                };
                std::process::exit(code);
            }
        }
    }

    #[cfg(not(unix))]
    {
        let status = std::process::Command::new(prog).args(args).status()?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

/// `favetto pair`: ask the daemon for a short-lived pairing code and print it.
async fn pair(args: cli::PairArgs) -> anyhow::Result<()> {
    pair::run(args).await
}

fn token_rotate(args: cli::TokenRotateArgs) -> anyhow::Result<()> {
    let data_dir = args.data_dir.unwrap_or_else(cli::default_data_dir);
    let path = data_dir.join("token");
    let token = favetto_core::auth::Token::generate();

    std::fs::create_dir_all(&data_dir)?;
    std::fs::write(&path, format!("{}\n", token.as_str()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    println!("new token written to {}", path.display());
    Ok(())
}
