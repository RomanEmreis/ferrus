use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::server::Role;

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
    Serve {
        /// Filter the exposed tool set by role (omit to expose all tools)
        #[arg(long, value_enum)]
        role: Option<Role>,
    },
    /// Write MCP config files so agents can launch ferrus automatically
    Register {
        /// Agent to configure as Supervisor
        #[arg(long, value_enum, value_name = "AGENT")]
        supervisor: commands::register::Agent,
        /// Agent to configure as Executor
        #[arg(long, value_enum, value_name = "AGENT")]
        executor: commands::register::Agent,
    },
}

impl Cli {
    pub async fn run(self) -> Result<()> {
        match self.command {
            Commands::Init => commands::init::run().await,
            Commands::Serve { role } => commands::serve::run(role).await,
            Commands::Register { supervisor, executor } => {
                commands::register::run(supervisor, executor).await
            }
        }
    }
}
