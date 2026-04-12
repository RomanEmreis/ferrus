use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::server::Role;

pub mod commands;

#[derive(Parser)]
#[command(
    name = "ferrus",
    about = "AI orchestration MCP server — coordinates Supervisor + Executor agents",
    version = env!("CARGO_PKG_VERSION"),
)]
pub struct Cli {
    /// Enable debug mode regardless of build profile
    #[arg(long, global = true)]
    debug: bool,

    #[command(subcommand)]
    command: Option<Commands>,
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
        /// Optional model override to store for the Supervisor
        #[arg(long, value_name = "MODEL")]
        supervisor_model: Option<String>,
        /// Agent to configure as Executor (optional if --supervisor is set)
        #[arg(long, value_enum, value_name = "AGENT")]
        executor: Option<commands::register::Agent>,
        /// Optional model override to store for the Executor
        #[arg(long, value_name = "MODEL")]
        executor_model: Option<String>,
    },
}

impl Cli {
    pub fn debug_enabled(&self) -> bool {
        cfg!(debug_assertions) || self.debug
    }

    pub fn is_hq_mode(&self) -> bool {
        self.command.is_none()
    }

    pub async fn run(self, debug: bool) -> Result<()> {
        match self.command {
            Some(Commands::Init { agents_path }) => commands::init::run(agents_path).await,
            Some(Commands::Serve {
                role,
                agent_name,
                agent_index,
            }) => commands::serve::run(role, agent_name, agent_index, debug).await,
            Some(Commands::Register {
                supervisor,
                supervisor_model,
                executor,
                executor_model,
            }) => {
                if supervisor.is_none() && executor.is_none() {
                    anyhow::bail!("At least one of --supervisor or --executor must be specified");
                }
                commands::register::run(supervisor, supervisor_model, executor, executor_model)
                    .await
            }
            None => crate::hq::run(debug).await,
        }
    }
}
