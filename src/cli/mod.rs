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
    Init {
        /// Root directory for agent skill files (default: .agents)
        #[arg(long, default_value = ".agents")]
        agents_path: String,
    },
    /// Start the MCP server on stdio
    Serve {
        /// Filter the exposed tool set by role (omit to expose all tools)
        #[arg(long, value_enum)]
        role: Option<Role>,
        /// Human-readable agent name embedded in the claimed_by field (e.g. "codex", "claude-code")
        #[arg(long, default_value = "unknown")]
        agent_name: String,
        /// Index disambiguating multiple agents of the same role and name (e.g. 1, 2)
        #[arg(long, default_value_t = 0u32)]
        agent_index: u32,
    },
    /// Write MCP config files so agents can launch ferrus automatically
    Register {
        /// Agent to configure as Supervisor (optional if --executor is set)
        #[arg(long, value_enum, value_name = "AGENT")]
        supervisor: Option<commands::register::Agent>,
        /// Agent to configure as Executor (optional if --supervisor is set)
        #[arg(long, value_enum, value_name = "AGENT")]
        executor: Option<commands::register::Agent>,
    },
}

impl Cli {
    pub async fn run(self) -> Result<()> {
        match self.command {
            Commands::Init { agents_path } => commands::init::run(agents_path).await,
            Commands::Serve {
                role,
                agent_name,
                agent_index,
            } => commands::serve::run(role, agent_name, agent_index).await,
            Commands::Register {
                supervisor,
                executor,
            } => {
                if supervisor.is_none() && executor.is_none() {
                    anyhow::bail!("At least one of --supervisor or --executor must be specified");
                }
                commands::register::run(supervisor, executor).await
            }
        }
    }
}
