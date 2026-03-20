use anyhow::{bail, Result};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "ferrus-hq", no_binary_name = true, disable_help_subcommand = true)]
struct HqCli {
    #[command(subcommand)]
    command: ShellCommand,
}

#[derive(Debug, Subcommand)]
pub enum ShellCommand {
    /// Show task state and agent list.
    Status,
    /// Reset all task files and set state to Idle (prompts for confirmation if state is Executing or Reviewing).
    Reset,
    /// Stop all running executor and supervisor/reviewer sessions (prompts for confirmation).
    Stop,
    /// Exit HQ.
    Quit,
    /// Spawn the supervisor and plan a task.
    Plan,
    /// Attach terminal to a running background session. Ctrl+] d to detach.
    Attach { name: String },
    /// Manually spawn supervisor in review mode (for the current Reviewing submission).
    Review,
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
    /// Show all available HQ commands.
    Help,
}

/// Parse `/command [args…]` into a `ShellCommand`.
pub fn parse_command(input: &str) -> Result<ShellCommand> {
    let input = input.trim();
    if !input.starts_with('/') {
        bail!("Commands must start with '/' — try /status, /plan, /quit");
    }
    let tokens = shlex::split(&input[1..])
        .ok_or_else(|| anyhow::anyhow!("Failed to tokenize command (unterminated quote?)"))?;
    let cli = HqCli::try_parse_from(tokens).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(cli.command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_quit() {
        assert!(matches!(
            parse_command("/quit").unwrap(),
            ShellCommand::Quit
        ));
    }
    #[test]
    fn parse_status() {
        assert!(matches!(
            parse_command("/status").unwrap(),
            ShellCommand::Status
        ));
    }
    #[test]
    fn parse_reset() {
        assert!(matches!(
            parse_command("/reset").unwrap(),
            ShellCommand::Reset
        ));
    }
    #[test]
    fn parse_stop() {
        assert!(matches!(
            parse_command("/stop").unwrap(),
            ShellCommand::Stop
        ));
    }
    #[test]
    fn parse_plan() {
        assert!(matches!(
            parse_command("/plan").unwrap(),
            ShellCommand::Plan
        ));
    }
    #[test]
    fn parse_review() {
        assert!(matches!(
            parse_command("/review").unwrap(),
            ShellCommand::Review
        ));
    }
    #[test]
    fn parse_attach_with_name() {
        match parse_command("/attach executor").unwrap() {
            ShellCommand::Attach { name } => assert_eq!(name, "executor"),
            _ => panic!("expected Attach"),
        }
    }
    #[test]
    fn attach_parses_hyphenated_name() {
        match parse_command("/attach executor-1").unwrap() {
            ShellCommand::Attach { name } => assert_eq!(name, "executor-1"),
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
