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
    fn mcp_config_entry(&self, role: &str, index: u32) -> Result<McpConfigEntry>;
}

pub trait ExecutorAgent: Send + Sync {
    fn name(&self) -> &'static str;
    fn spawn(&self, prompt: Option<&str>) -> Command;
    fn spawn_headlessly(&self, prompt: &str) -> Command;
    fn mcp_config_entry(&self, role: &str, index: u32) -> Result<McpConfigEntry>;
}

#[derive(Debug, Clone, Copy)]
pub struct ClaudeCodeSupervisor;

#[derive(Debug, Clone, Copy)]
pub struct ClaudeCodeExecutor;

#[derive(Debug, Clone, Copy)]
pub struct CodexSupervisor;

#[derive(Debug, Clone, Copy)]
pub struct CodexExecutor;

pub fn parse_supervisor_agent(agent_type: &str) -> Result<Arc<dyn SupervisorAgent>> {
    match agent_type {
        "claude-code" => Ok(Arc::new(ClaudeCodeSupervisor)),
        "codex" => Ok(Arc::new(CodexSupervisor)),
        other => bail!(
            "Unknown supervisor agent '{other}'. Supported values: \"claude-code\", \"codex\"."
        ),
    }
}

pub fn parse_executor_agent(agent_type: &str) -> Result<Arc<dyn ExecutorAgent>> {
    match agent_type {
        "claude-code" => Ok(Arc::new(ClaudeCodeExecutor)),
        "codex" => Ok(Arc::new(CodexExecutor)),
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

fn codex_headless_command(prompt: &str) -> Command {
    let mut cmd = Command::new("codex");
    cmd.arg("exec").arg(prompt);
    cmd
}

fn claude_headless_command(prompt: &str) -> Command {
    let mut cmd = Command::new("claude");
    cmd.arg("-p").arg(prompt);
    cmd
}

impl SupervisorAgent for ClaudeCodeSupervisor {
    fn name(&self) -> &'static str {
        "claude-code"
    }

    fn spawn(&self, prompt: Option<&str>) -> Command {
        positional_prompt_command("claude", prompt)
    }

    fn spawn_headlessly(&self, prompt: &str) -> Command {
        claude_headless_command(prompt)
    }

    fn mcp_config_entry(&self, role: &str, index: u32) -> Result<McpConfigEntry> {
        Ok(McpConfigEntry {
            command: current_exe_string()?,
            args: serve_args(role, self.name(), index),
        })
    }
}

impl ExecutorAgent for ClaudeCodeExecutor {
    fn name(&self) -> &'static str {
        "claude-code"
    }

    fn spawn(&self, prompt: Option<&str>) -> Command {
        positional_prompt_command("claude", prompt)
    }

    fn spawn_headlessly(&self, prompt: &str) -> Command {
        claude_headless_command(prompt)
    }

    fn mcp_config_entry(&self, role: &str, index: u32) -> Result<McpConfigEntry> {
        Ok(McpConfigEntry {
            command: current_exe_string()?,
            args: serve_args(role, self.name(), index),
        })
    }
}

impl SupervisorAgent for CodexSupervisor {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn spawn(&self, prompt: Option<&str>) -> Command {
        positional_prompt_command("codex", prompt)
    }

    fn spawn_headlessly(&self, prompt: &str) -> Command {
        codex_headless_command(prompt)
    }

    fn mcp_config_entry(&self, role: &str, index: u32) -> Result<McpConfigEntry> {
        Ok(McpConfigEntry {
            command: current_exe_string()?,
            args: serve_args(role, self.name(), index),
        })
    }
}

impl ExecutorAgent for CodexExecutor {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn spawn(&self, prompt: Option<&str>) -> Command {
        positional_prompt_command("codex", prompt)
    }

    fn spawn_headlessly(&self, prompt: &str) -> Command {
        codex_headless_command(prompt)
    }

    fn mcp_config_entry(&self, role: &str, index: u32) -> Result<McpConfigEntry> {
        Ok(McpConfigEntry {
            command: current_exe_string()?,
            args: serve_args(role, self.name(), index),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_program_and_args(command: Command, program: &str, args: &[&str]) {
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
    fn claude_supervisor_builds_interactive_command() {
        let agent = ClaudeCodeSupervisor;
        assert_program_and_args(agent.spawn(Some("plan")), "claude", &["plan"]);
    }

    #[test]
    fn claude_executor_builds_headless_command() {
        let agent = ClaudeCodeExecutor;
        assert_program_and_args(agent.spawn_headlessly("run"), "claude", &["-p", "run"]);
    }

    #[test]
    fn codex_supervisor_builds_interactive_command() {
        let agent = CodexSupervisor;
        assert_program_and_args(agent.spawn(Some("plan")), "codex", &["plan"]);
    }

    #[test]
    fn codex_executor_builds_headless_command() {
        let agent = CodexExecutor;
        assert_program_and_args(agent.spawn_headlessly("run"), "codex", &["exec", "run"]);
    }

    #[test]
    fn claude_config_entry_uses_expected_args() {
        let entry = ClaudeCodeSupervisor
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
    }

    #[test]
    fn claude_executor_config_entry_uses_expected_args() {
        let entry = ClaudeCodeExecutor.mcp_config_entry("executor", 4).unwrap();
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

    #[test]
    fn codex_supervisor_config_entry_uses_expected_args() {
        let entry = CodexSupervisor.mcp_config_entry("supervisor", 1).unwrap();
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
        let entry = CodexExecutor.mcp_config_entry("executor", 3).unwrap();
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
