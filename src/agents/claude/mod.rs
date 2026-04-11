use super::{positional_prompt_command, ExecutorAgent, SupervisorAgent};
use std::process::Command;

pub(super) const NAME: &str = "claude-code";
const EXECUTABLE: &str = "claude";

#[derive(Debug, Clone, Copy)]
pub struct Supervisor;

#[derive(Debug, Clone, Copy)]
pub struct Executor;

impl SupervisorAgent for Supervisor {
    fn name(&self) -> &'static str {
        NAME
    }

    fn spawn(&self, prompt: Option<&str>) -> Command {
        positional_prompt_command(EXECUTABLE, prompt)
    }

    fn spawn_headlessly(&self, prompt: &str) -> Command {
        claude_headless_command(prompt)
    }
}

impl ExecutorAgent for Executor {
    fn name(&self) -> &'static str {
        NAME
    }

    fn spawn(&self, prompt: Option<&str>) -> Command {
        positional_prompt_command(EXECUTABLE, prompt)
    }

    fn spawn_headlessly(&self, prompt: &str) -> Command {
        claude_headless_command(prompt)
    }
}

#[inline(always)]
fn claude_headless_command(prompt: &str) -> Command {
    let mut cmd = Command::new(EXECUTABLE);
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
