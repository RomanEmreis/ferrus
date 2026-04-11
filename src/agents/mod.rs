mod claude;
mod codex;

use anyhow::{bail, Context, Result};
use std::process::Command;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConfigEntry {
    pub command: String,
    pub args: Vec<String>,
}

pub trait SupervisorAgent: Send + Sync {
    fn name(&self) -> &'static str;
    fn spawn(&self, prompt: Option<&str>) -> Command;
    fn spawn_headlessly(&self, prompt: &str) -> Command;
    fn mcp_config_entry(&self, role: &str, index: u32) -> Result<McpConfigEntry> {
        Ok(McpConfigEntry {
            command: current_exe_string()?,
            args: serve_args(role, self.name(), index),
        })
    }
}

pub trait ExecutorAgent: Send + Sync {
    fn name(&self) -> &'static str;
    fn spawn(&self, prompt: Option<&str>) -> Command;
    fn spawn_headlessly(&self, prompt: &str) -> Command;
    fn mcp_config_entry(&self, role: &str, index: u32) -> Result<McpConfigEntry> {
        Ok(McpConfigEntry {
            command: current_exe_string()?,
            args: serve_args(role, self.name(), index),
        })
    }
}

pub fn parse_supervisor_agent(agent_type: &str) -> Result<Arc<dyn SupervisorAgent>> {
    match agent_type {
        claude::NAME => Ok(Arc::new(claude::Supervisor)),
        codex::NAME => Ok(Arc::new(codex::Supervisor)),
        other => bail!(
            "Unknown supervisor agent '{other}'. Supported values: \"claude-code\", \"codex\"."
        ),
    }
}

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
    Ok(std::env::current_exe()
        .context("Failed to resolve current executable path")?
        .to_string_lossy()
        .into_owned())
}

fn serve_args(role: &str, agent_name: &str, index: u32) -> Vec<String> {
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
