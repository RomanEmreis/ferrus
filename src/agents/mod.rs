//! Agent adapters for the supported supervisor and executor backends.
//!
//! This module centralizes how Ferrus names agent implementations, spawns them
//! interactively or headlessly, and derives the MCP configuration used by HQ.

pub(crate) mod claude;
pub(crate) mod codex;

use anyhow::{bail, Context, Result};
use std::process::Command;
use std::sync::Arc;

/// Describes one MCP server entry for a spawned Ferrus agent.
///
/// Ferrus writes these values into client-facing configuration so external
/// tools can reconnect to the correct `ferrus serve` process for a given role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConfigEntry {
    /// Executable that should be launched for the MCP server.
    pub command: String,
    /// Arguments passed to [`Self::command`] when the MCP server starts.
    pub args: Vec<String>,
}

/// Behavior required from a supervisor-capable agent implementation.
///
/// Supervisor agents support both interactive sessions for humans and
/// headless sessions for HQ-managed workflows.
pub trait SupervisorAgent: Send + Sync {
    /// Returns the stable configuration name for this agent backend.
    fn name(&self) -> &'static str;

    /// Builds the interactive command used when a human drives the supervisor.
    fn spawn(&self, prompt: Option<&str>) -> Command;

    /// Builds the non-interactive command used when HQ drives the supervisor.
    fn spawn_headlessly(&self, prompt: &str) -> Command;

    /// Builds the MCP configuration entry for this supervisor instance.
    ///
    /// The default implementation points the client back at the current
    /// `ferrus` executable so HQ can serve tools through the repository's own
    /// binary rather than relying on an external wrapper.
    ///
    /// # Errors
    ///
    /// Returns an error when Ferrus cannot resolve the path to the current
    /// executable.
    fn mcp_config_entry(&self, role: &str, index: u32) -> Result<McpConfigEntry> {
        Ok(McpConfigEntry {
            command: current_exe_string()?,
            args: serve_args(role, self.name(), index),
        })
    }
}

/// Behavior required from an executor-capable agent implementation.
///
/// Executors mirror the supervisor API because HQ may start them in interactive
/// or headless modes depending on the orchestration context.
pub trait ExecutorAgent: Send + Sync {
    /// Returns the stable configuration name for this agent backend.
    fn name(&self) -> &'static str;

    /// Builds the interactive command used when a human drives the executor.
    fn spawn(&self, prompt: Option<&str>) -> Command;

    /// Builds the non-interactive command used when HQ drives the executor.
    fn spawn_headlessly(&self, prompt: &str) -> Command;

    /// Builds the MCP configuration entry for this executor instance.
    ///
    /// # Errors
    ///
    /// Returns an error when Ferrus cannot resolve the path to the current
    /// executable.
    fn mcp_config_entry(&self, role: &str, index: u32) -> Result<McpConfigEntry> {
        Ok(McpConfigEntry {
            command: current_exe_string()?,
            args: serve_args(role, self.name(), index),
        })
    }
}

/// Parses a configured supervisor agent name into its concrete implementation.
///
/// # Errors
///
/// Returns an error that lists the supported agent names when `agent_type` does
/// not match a registered supervisor backend.
pub fn parse_supervisor_agent(agent_type: &str) -> Result<Arc<dyn SupervisorAgent>> {
    match agent_type {
        claude::NAME => Ok(Arc::new(claude::Supervisor)),
        codex::NAME => Ok(Arc::new(codex::Supervisor)),
        other => bail!(
            "Unknown supervisor agent '{other}'. Supported values: \"claude-code\", \"codex\"."
        ),
    }
}

/// Parses a configured executor agent name into its concrete implementation.
///
/// # Errors
///
/// Returns an error that lists the supported agent names when `agent_type` does
/// not match a registered executor backend.
pub fn parse_executor_agent(agent_type: &str) -> Result<Arc<dyn ExecutorAgent>> {
    match agent_type {
        claude::NAME => Ok(Arc::new(claude::Executor)),
        codex::NAME => Ok(Arc::new(codex::Executor)),
        other => {
            bail!("Unknown executor agent '{other}'. Supported values: \"claude-code\", \"codex\".")
        }
    }
}

fn current_exe_string() -> Result<String> {
    // Persist the exact executable path so generated MCP configs keep working
    // even when Ferrus is launched outside of PATH-based resolution.
    Ok(std::env::current_exe()
        .context("Failed to resolve current executable path")?
        .to_string_lossy()
        .into_owned())
}

fn serve_args(role: &str, agent_name: &str, index: u32) -> Vec<String> {
    // Ferrus reconnects to agents through `ferrus serve`, so every backend uses
    // the same argument shape with role and agent identity baked in.
    vec![
        "serve".to_string(),
        "--role".to_string(),
        role.to_string(),
        "--agent-name".to_string(),
        agent_name.to_string(),
        "--agent-index".to_string(),
        index.to_string(),
    ]
}

fn positional_prompt_command(binary: &str, prompt: Option<&str>) -> Command {
    let mut cmd = Command::new(binary);
    if let Some(prompt) = prompt {
        // Both supported CLIs accept an optional positional prompt for
        // interactive startup, so we only append it when the caller supplied one.
        cmd.arg(prompt);
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn assert_program_and_args(command: Command, program: &str, args: &[&str]) {
        assert_eq!(command.get_program().to_string_lossy(), program);
        let actual = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let expected = args
            .iter()
            .map(|arg| (*arg).to_string())
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    #[test]
    fn unknown_supervisor_agent_is_actionable() {
        let err = match parse_supervisor_agent("unknown") {
            Ok(_) => panic!("expected unknown supervisor agent to fail"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("Unknown supervisor agent 'unknown'"));
        assert!(err.contains("claude-code"));
        assert!(err.contains("codex"));
    }

    #[test]
    fn unknown_executor_agent_is_actionable() {
        let err = match parse_executor_agent("unknown") {
            Ok(_) => panic!("expected unknown executor agent to fail"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("Unknown executor agent 'unknown'"));
        assert!(err.contains("claude-code"));
        assert!(err.contains("codex"));
    }
}
