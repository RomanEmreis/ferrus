//! Codex-backed supervisor and executor adapters.
//!
//! These wrappers isolate the CLI details needed to launch Codex in the shapes
//! Ferrus expects for interactive and headless sessions.

use super::{positional_prompt_command, ExecutorAgent, SupervisorAgent};
use std::process::Command;

/// Stable agent identifier used in Ferrus configuration and error messages.
pub(super) const NAME: &str = "codex";

/// Interactive and headless supervisor launcher for the Codex CLI.
#[derive(Debug, Clone, Copy)]
pub struct Supervisor;

/// Interactive and headless executor launcher for the Codex CLI.
#[derive(Debug, Clone, Copy)]
pub struct Executor;

impl SupervisorAgent for Supervisor {
    /// Returns the Ferrus-visible identifier for the Codex supervisor backend.
    fn name(&self) -> &'static str {
        NAME
    }

    /// Builds the interactive Codex command, optionally seeding it with a prompt.
    fn spawn(&self, prompt: Option<&str>) -> Command {
        positional_prompt_command(NAME, prompt)
    }

    /// Builds the headless Codex command used by Ferrus HQ.
    fn spawn_headlessly(&self, prompt: &str) -> Command {
        codex_headless_command(prompt)
    }
}

impl ExecutorAgent for Executor {
    /// Returns the Ferrus-visible identifier for the Codex executor backend.
    fn name(&self) -> &'static str {
        NAME
    }

    /// Builds the interactive Codex command, optionally seeding it with a prompt.
    fn spawn(&self, prompt: Option<&str>) -> Command {
        positional_prompt_command(NAME, prompt)
    }

    /// Builds the headless Codex command used by Ferrus HQ.
    fn spawn_headlessly(&self, prompt: &str) -> Command {
        codex_headless_command(prompt)
    }
}

#[inline(always)]
fn codex_headless_command(prompt: &str) -> Command {
    let mut cmd = Command::new(NAME);
    // `codex exec` is the non-interactive entrypoint that runs a single prompt
    // and exits, which matches Ferrus executor and supervisor automation.
    cmd.arg("exec").arg(prompt);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::tests::assert_program_and_args;

    #[test]
    fn codex_supervisor_builds_interactive_command() {
        let agent = Supervisor;
        assert_program_and_args(agent.spawn(Some("plan")), "codex", &["plan"]);
    }

    #[test]
    fn codex_executor_builds_headless_command() {
        let agent = Executor;
        assert_program_and_args(agent.spawn_headlessly("run"), "codex", &["exec", "run"]);
    }

    #[test]
    fn codex_supervisor_config_entry_uses_expected_args() {
        let entry = Supervisor.mcp_config_entry("supervisor", 1).unwrap();
        assert!(!entry.command.is_empty());
        assert_eq!(
            entry.args,
            vec![
                "serve",
                "--role",
                "supervisor",
                "--agent-name",
                "codex",
                "--agent-index",
                "1",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn codex_config_entry_uses_expected_args() {
        let entry = Executor.mcp_config_entry("executor", 3).unwrap();
        assert!(!entry.command.is_empty());
        assert_eq!(
            entry.args,
            vec![
                "serve",
                "--role",
                "executor",
                "--agent-name",
                "codex",
                "--agent-index",
                "3",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
    }
}
