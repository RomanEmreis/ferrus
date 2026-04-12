//! Claude Code-backed supervisor and executor adapters.
//!
//! Ferrus uses this module to normalize Claude's CLI conventions so the rest of
//! the orchestration layer can treat it like any other agent backend.

use super::{positional_prompt_command, ExecutorAgent, SupervisorAgent};
use std::process::Command;

/// Stable agent identifier used in Ferrus configuration and error messages.
pub(crate) const NAME: &str = "claude-code";
/// Actual CLI executable name used to launch Claude Code.
const EXECUTABLE: &str = "claude";

/// Interactive and headless supervisor launcher for Claude Code.
#[derive(Debug, Clone, Copy)]
pub struct Supervisor;

/// Interactive and headless executor launcher for Claude Code.
#[derive(Debug, Clone, Copy)]
pub struct Executor;

impl SupervisorAgent for Supervisor {
    /// Returns the Ferrus-visible identifier for the Claude supervisor backend.
    fn name(&self) -> &'static str {
        NAME
    }

    /// Builds the interactive Claude command, optionally seeding it with a prompt.
    fn spawn(&self, prompt: Option<&str>) -> Command {
        positional_prompt_command(EXECUTABLE, prompt)
    }

    /// Builds the headless Claude command used by Ferrus HQ.
    fn spawn_headlessly(&self, prompt: &str) -> Command {
        claude_headless_command(prompt)
    }
}

impl ExecutorAgent for Executor {
    /// Returns the Ferrus-visible identifier for the Claude executor backend.
    fn name(&self) -> &'static str {
        NAME
    }

    /// Builds the interactive Claude command, optionally seeding it with a prompt.
    fn spawn(&self, prompt: Option<&str>) -> Command {
        positional_prompt_command(EXECUTABLE, prompt)
    }

    /// Builds the headless Claude command used by Ferrus HQ.
    fn spawn_headlessly(&self, prompt: &str) -> Command {
        claude_headless_command(prompt)
    }
}

#[inline(always)]
fn claude_headless_command(prompt: &str) -> Command {
    let mut cmd = Command::new(EXECUTABLE);
    // Claude's print-and-exit flow is exposed as `-p`, so Ferrus uses that
    // form to run headless tasks without opening an interactive TUI.
    cmd.arg("-p").arg(prompt);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::tests::assert_program_and_args;

    #[test]
    fn claude_supervisor_builds_interactive_command() {
        let agent = Supervisor;
        assert_program_and_args(agent.spawn(Some("plan")), "claude", &["plan"]);
    }

    #[test]
    fn claude_executor_builds_headless_command() {
        let agent = Executor;
        assert_program_and_args(agent.spawn_headlessly("run"), "claude", &["-p", "run"]);
    }

    #[test]
    fn claude_config_entry_uses_expected_args() {
        let entry = Supervisor.mcp_config_entry("supervisor", 2).unwrap();
        assert!(!entry.command.is_empty());
        assert_eq!(
            entry.args,
            vec![
                "serve",
                "--role",
                "supervisor",
                "--agent-name",
                "claude-code",
                "--agent-index",
                "2",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn claude_executor_config_entry_uses_expected_args() {
        let entry = Executor.mcp_config_entry("executor", 4).unwrap();
        assert!(!entry.command.is_empty());
        assert_eq!(
            entry.args,
            vec![
                "serve",
                "--role",
                "executor",
                "--agent-name",
                "claude-code",
                "--agent-index",
                "4",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
    }
}
