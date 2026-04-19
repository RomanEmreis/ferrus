mod agent_id;
mod agents;
mod checks;
mod cli;
mod config;
mod hq;
mod platform;
mod server;
mod state;
mod update_check;

use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = cli::Cli::parse();
    let debug = args.debug_enabled();

    if args.is_hq_mode() {
        init_hq_logger();
    } else {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("ferrus=info"));
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
    }

    args.run(debug).await
}

fn init_hq_logger() {
    // Log to file in HQ mode so the terminal isn't cluttered.
    // Best-effort: if .ferrus/ doesn't exist yet, logging goes nowhere.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("off"));
    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(".ferrus/hq.log")
    {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::sync::Mutex::new(file))
            .try_init();
    }
}
