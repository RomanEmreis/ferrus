use anyhow::Result;
use clap::{Parser, Subcommand};

mod commands;

#[derive(Parser)]
#[command(
    name = "ferrus",
    about = "AI orchestration MCP server — coordinates Supervisor + Executor agents"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize ferrus in the current directory (creates ferrus.toml and .ferrus/)
    Init,
    /// Start the MCP server on stdio
    Serve,
}

impl Cli {
    pub async fn run(self) -> Result<()> {
        match self.command {
            Commands::Init => commands::init::run().await,
            Commands::Serve => commands::serve::run().await,
        }
    }
}
