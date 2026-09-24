//! Standalone `favetto-tui` binary: attach the TUI to a running daemon.

use clap::Parser;

use favetto_tui::cli::TuiArgs;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "favetto=info".into()),
        )
        .init();

    let Cli { args } = Cli::parse();
    favetto_tui::tui::run(args).await
}

/// `favetto-tui`'s whole command line is the flattened [`TuiArgs`].
#[derive(Parser)]
#[command(
    name = "favetto-tui",
    version,
    about = "Attach the favetto TUI to a running daemon"
)]
struct Cli {
    #[command(flatten)]
    args: TuiArgs,
}
