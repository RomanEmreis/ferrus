use anyhow::{bail, Result};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "ferrus-hq", no_binary_name = true)]
struct HqCli {
    #[command(subcommand)]
    command: ShellCommand,
}

#[derive(Debug, Subcommand)]
pub enum ShellCommand {
    /// Show task state and agent list.
    Status,
    /// Exit HQ.
    Quit,
    /// Spawn the supervisor and plan a task.
    Plan,
    /// Attach to a running agent's terminal (Phase B).
    Attach { name: String },
    /// Initialize ferrus in the current directory.
    Init {
        #[arg(long, default_value = ".agents")]
        agents_path: String,
    },
    /// Register agents (same as `ferrus register`).
    Register {
        #[arg(long, value_name = "AGENT")]
        supervisor: Option<String>,
        #[arg(long, value_name = "AGENT")]
        executor: Option<String>,
    },
}

/// Parse `/command [args…]` into a `ShellCommand`.
pub fn parse_command(input: &str) -> Result<ShellCommand> {
    let input = input.trim();
    if !input.starts_with('/') {
        bail!("Commands must start with '/' — try /status, /plan, /quit");
    }
    let tokens = shlex::split(&input[1..])
        .ok_or_else(|| anyhow::anyhow!("Failed to tokenize command (unterminated quote?)"))?;
    let cli = HqCli::try_parse_from(tokens)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(cli.command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_quit() {
        assert!(matches!(parse_command("/quit").unwrap(), ShellCommand::Quit));
    }
    #[test]
    fn parse_status() {
        assert!(matches!(parse_command("/status").unwrap(), ShellCommand::Status));
    }
    #[test]
    fn parse_plan() {
        assert!(matches!(parse_command("/plan").unwrap(), ShellCommand::Plan));
    }
    #[test]
    fn parse_attach_with_name() {
        match parse_command("/attach executor").unwrap() {
            ShellCommand::Attach { name } => assert_eq!(name, "executor"),
            _ => panic!("expected Attach"),
        }
    }
    #[test]
    fn unknown_command_errors() {
        assert!(parse_command("/foobar").is_err());
    }
    #[test]
    fn non_slash_errors() {
        assert!(parse_command("hello").is_err());
    }
}
