//! Codex-backed supervisor and executor adapters.
//!
//! These wrappers isolate the CLI details needed to launch Codex in the shapes
//! Ferrus expects for interactive and headless sessions.

use super::{normalized_model, AgentRunMode, ExecutorAgent, SupervisorAgent};
use std::process::Command;

/// Stable agent identifier used in Ferrus configuration and error messages.
pub(crate) const NAME: &str = "codex";

/// Interactive and headless supervisor launcher for the Codex CLI.
#[derive(Debug, Clone)]
pub struct Supervisor {
    model: Option<String>,
}

/// Interactive and headless executor launcher for the Codex CLI.
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
    /// Returns the Ferrus-visible identifier for the Codex supervisor backend.
    fn name(&self) -> &'static str {
        NAME
    }

    /// Builds the Codex command used by Ferrus HQ or an interactive user.
    fn spawn(&self, mode: AgentRunMode<'_>) -> Command {
        codex_command(mode, self.model())
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }
}

impl ExecutorAgent for Executor {
    /// Returns the Ferrus-visible identifier for the Codex executor backend.
    fn name(&self) -> &'static str {
        NAME
    }

    /// Builds the Codex command used by Ferrus HQ or an interactive user.
    fn spawn(&self, mode: AgentRunMode<'_>) -> Command {
        codex_command(mode, self.model())
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }
}

#[inline(always)]
fn codex_command(mode: AgentRunMode<'_>, model: Option<&str>) -> Command {
    let mut cmd = Command::new(NAME);
    match mode {
        AgentRunMode::Interactive { prompt } => {
            if let Some(model) = model {
                cmd.arg("--model").arg(model);
            }
            if let Some(prompt) = prompt {
                cmd.arg(prompt);
            }
        }
        AgentRunMode::Headless { prompt } => {
            // `codex exec` is the non-interactive entrypoint that runs a single prompt
            // and exits, which matches Ferrus executor and supervisor automation.
            cmd.arg("exec");
            if let Some(model) = model {
                cmd.arg("--model").arg(model);
            }
            cmd.arg(prompt);
        }
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::tests::assert_program_and_args;

    #[test]
    fn codex_supervisor_builds_interactive_command() {
        let agent = Supervisor::new(None);
        assert_program_and_args(
            agent.spawn(AgentRunMode::Interactive {
                prompt: Some("plan"),
            }),
            "codex",
            &["plan"],
        );
    }

    #[test]
    fn codex_executor_builds_headless_command() {
        let agent = Executor::new(None);
        assert_program_and_args(
            agent.spawn(AgentRunMode::Headless { prompt: "run" }),
            "codex",
            &["exec", "run"],
        );
    }

    #[test]
    fn codex_model_override_is_part_of_spawned_command() {
        let agent = Executor::new(Some("gpt-5.4"));
        assert_program_and_args(
            agent.spawn(AgentRunMode::Headless { prompt: "run" }),
            "codex",
            &["exec", "--model", "gpt-5.4", "run"],
        );
    }

    #[test]
    fn codex_supervisor_config_entry_uses_expected_args() {
        let entry = Supervisor::new(Some("gpt-5.4"))
            .mcp_config_entry("supervisor", 1)
            .unwrap();
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
        assert_eq!(entry.model.as_deref(), Some("gpt-5.4"));
    }

    #[test]
    fn codex_config_entry_uses_expected_args() {
        let entry = Executor::new(None).mcp_config_entry("executor", 3).unwrap();
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
        assert_eq!(entry.model, None);
    }
}
