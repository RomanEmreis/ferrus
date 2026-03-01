mod checks;
mod cli;
mod config;
mod server;
mod state;

use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Write logs to stderr so they don't corrupt the stdio MCP stream.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("ferrus=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    cli::Cli::parse().run().await
}
