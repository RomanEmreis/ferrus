//! Qwen Code-backed supervisor and executor adapters.
//!
//! Ferrus uses this module to normalize Qwen's CLI conventions so the rest of
//! the orchestration layer can treat it like any other agent backend.

use super::{AgentRunMode, ExecutorAgent, SupervisorAgent, normalized_model};
use crate::platform;
use std::process::Command;

/// Stable agent identifier used in Ferrus configuration and error messages.
pub(crate) const NAME: &str = "qwen-code";
/// Actual CLI executable name used to launch Qwen Code.
const EXECUTABLE: &str = "qwen";

/// Interactive and headless supervisor launcher for Qwen Code.
#[derive(Debug, Clone)]
pub struct Supervisor {
    model: Option<String>,
}

/// Interactive and headless executor launcher for Qwen Code.
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
    /// Returns the Ferrus-visible identifier for the Qwen supervisor backend.
    fn name(&self) -> &'static str {
        NAME
    }

    /// Builds the Qwen command used by Ferrus HQ or an interactive user.
    fn spawn(&self, mode: AgentRunMode<'_>) -> Command {
        qwen_command(mode, self.model())
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }
}

impl ExecutorAgent for Executor {
    /// Returns the Ferrus-visible identifier for the Qwen executor backend.
    fn name(&self) -> &'static str {
        NAME
    }

    /// Builds the Qwen command used by Ferrus HQ or an interactive user.
    fn spawn(&self, mode: AgentRunMode<'_>) -> Command {
        qwen_command(mode, self.model())
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }
}

#[inline(always)]
fn qwen_command(mode: AgentRunMode<'_>, model: Option<&str>) -> Command {
    let mut cmd = platform::agent_command(EXECUTABLE);
    if let Some(model) = model {
        cmd.arg("--model").arg(model);
    }
    match mode {
        AgentRunMode::Interactive { prompt } => {
            if let Some(prompt) = prompt {
                cmd.arg("-i").arg(prompt);
            }
        }
        AgentRunMode::Headless { prompt } => {
            // Qwen follows Claude's print-and-exit pattern via `-p`.
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
    fn qwen_supervisor_builds_interactive_command() {
        let agent = Supervisor::new(None);
        assert_program_and_args(
            agent.spawn(AgentRunMode::Interactive {
                prompt: Some("plan"),
            }),
            "qwen",
            &["-i", "plan"],
        );
    }

    #[test]
    fn qwen_executor_builds_headless_command() {
        let agent = Executor::new(None);
        assert_program_and_args(
            agent.spawn(AgentRunMode::Headless { prompt: "run" }),
            "qwen",
            &["-p", "run"],
        );
    }

    #[test]
    fn qwen_model_override_is_part_of_spawned_command() {
        let agent = Supervisor::new(Some("qwen3-coder-plus"));
        assert_program_and_args(
            agent.spawn(AgentRunMode::Headless { prompt: "review" }),
            "qwen",
            &["--model", "qwen3-coder-plus", "-p", "review"],
        );
    }

    #[test]
    fn qwen_config_entry_uses_expected_args() {
        let entry = Supervisor::new(Some("qwen3-coder-plus"))
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
                "qwen-code",
                "--agent-index",
                "2",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
        assert_eq!(entry.model.as_deref(), Some("qwen3-coder-plus"));
    }

    #[test]
    fn qwen_executor_config_entry_uses_expected_args() {
        let entry = Executor::new(None).mcp_config_entry("executor", 4).unwrap();
        assert!(!entry.command.is_empty());
        assert_eq!(
            entry.args,
            vec![
                "serve",
                "--role",
                "executor",
                "--agent-name",
                "qwen-code",
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
