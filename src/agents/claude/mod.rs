//! Claude Code-backed supervisor and executor adapters.
//!
//! Ferrus uses this module to normalize Claude's CLI conventions so the rest of
//! the orchestration layer can treat it like any other agent backend.

use super::{AgentRunMode, ExecutorAgent, SupervisorAgent, normalized_model};
use crate::platform;
use std::process::Command;

/// Stable agent identifier used in Ferrus configuration and error messages.
pub(crate) const NAME: &str = "claude-code";
/// Actual CLI executable name used to launch Claude Code.
const EXECUTABLE: &str = "claude";

/// Interactive and headless supervisor launcher for Claude Code.
#[derive(Debug, Clone)]
pub struct Supervisor {
    model: Option<String>,
}

/// Interactive and headless executor launcher for Claude Code.
#[derive(Debug, Clone)]
pub struct Executor {
    model: Option<String>,
}

impl Supervisor {
    pub fn new(model: Option<&str>) -> Self {
        Self {
            model: normalized_model(model),
        }
    }
}

impl Executor {
    pub fn new(model: Option<&str>) -> Self {
        Self {
            model: normalized_model(model),
        }
    }
}

impl SupervisorAgent for Supervisor {
    /// Returns the Ferrus-visible identifier for the Claude supervisor backend.
    fn name(&self) -> &'static str {
        NAME
    }

    /// Builds the Claude command used by Ferrus HQ or an interactive user.
    fn spawn(&self, mode: AgentRunMode<'_>) -> Command {
        claude_command(mode, self.model())
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }
}

impl ExecutorAgent for Executor {
    /// Returns the Ferrus-visible identifier for the Claude executor backend.
    fn name(&self) -> &'static str {
        NAME
    }

    /// Builds the Claude command used by Ferrus HQ or an interactive user.
    fn spawn(&self, mode: AgentRunMode<'_>) -> Command {
        claude_command(mode, self.model())
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }
}

#[inline(always)]
fn claude_command(mode: AgentRunMode<'_>, model: Option<&str>) -> Command {
    let mut cmd = platform::agent_command(EXECUTABLE);
    if let Some(model) = model {
        cmd.arg("--model").arg(model);
    }
    match mode {
        AgentRunMode::Interactive { prompt } => {
            if let Some(prompt) = prompt {
                cmd.arg(prompt);
            }
        }
        AgentRunMode::Headless { prompt } => {
            // Claude's print-and-exit flow is exposed as `-p`, so Ferrus uses that
            // form to run headless tasks without opening an interactive TUI.
            cmd.arg("-p").arg(prompt);
        }
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::tests::assert_program_and_args;

    #[test]
    fn claude_supervisor_builds_interactive_command() {
        let agent = Supervisor::new(None);
        assert_program_and_args(
            agent.spawn(AgentRunMode::Interactive {
                prompt: Some("plan"),
            }),
            "claude",
            &["plan"],
        );
    }

    #[test]
    fn claude_executor_builds_headless_command() {
        let agent = Executor::new(None);
        assert_program_and_args(
            agent.spawn(AgentRunMode::Headless { prompt: "run" }),
            "claude",
            &["-p", "run"],
        );
    }

    #[test]
    fn claude_model_override_is_part_of_spawned_command() {
        let agent = Supervisor::new(Some("claude-opus-4-6"));
        assert_program_and_args(
            agent.spawn(AgentRunMode::Headless { prompt: "review" }),
            "claude",
            &["--model", "claude-opus-4-6", "-p", "review"],
        );
    }

    #[test]
    fn claude_config_entry_uses_expected_args() {
        let entry = Supervisor::new(Some("claude-opus-4-6"))
            .mcp_config_entry("supervisor", 2)
            .unwrap();
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
        assert_eq!(entry.model.as_deref(), Some("claude-opus-4-6"));
    }

    #[test]
    fn claude_executor_config_entry_uses_expected_args() {
        let entry = Executor::new(None).mcp_config_entry("executor", 4).unwrap();
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
        assert_eq!(entry.model, None);
    }
}
