use anyhow::{Context, Result};
use tokio::process::Command;

pub struct CommandResult {
    pub command: String,
    pub passed: bool,
    pub stdout: String,
    pub stderr: String,
}

pub struct CheckResult {
    pub passed: bool,
    pub commands: Vec<CommandResult>,
}

/// Run every configured check command in order, collecting stdout/stderr for each.
pub async fn run_checks(commands: &[String]) -> Result<CheckResult> {
    let mut results = Vec::with_capacity(commands.len());
    let mut passed = true;

    for cmd in commands {
        let result = run_command(cmd)
            .await
            .with_context(|| format!("Failed to spawn command: {cmd}"))?;
        if !result.passed {
            passed = false;
        }
        results.push(result);
    }

    Ok(CheckResult {
        passed,
        commands: results,
    })
}

async fn run_command(cmd: &str) -> Result<CommandResult> {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    let (program, args) = match parts.split_first() {
        Some(pair) => pair,
        None => {
            return Ok(CommandResult {
                command: cmd.to_string(),
                passed: true,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    };

    let output = Command::new(program)
        .args(args)
        .output()
        .await
        .with_context(|| format!("Failed to run `{program}`"))?;

    Ok(CommandResult {
        command: cmd.to_string(),
        passed: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_checks_with_single_passing_command() {
        let commands = vec!["true".to_string()];

        let result = run_checks(&commands).await.unwrap();

        assert!(result.passed);
        assert_eq!(result.commands.len(), 1);
        assert_eq!(result.commands[0].command, "true");
        assert!(result.commands[0].passed);
    }

    #[tokio::test]
    async fn run_checks_with_single_failing_command() {
        let commands = vec!["false".to_string()];

        let result = run_checks(&commands).await.unwrap();

        assert!(!result.passed);
        assert_eq!(result.commands.len(), 1);
        assert_eq!(result.commands[0].command, "false");
        assert!(!result.commands[0].passed);
    }

    #[tokio::test]
    async fn run_checks_with_mixed_commands_collects_all_results() {
        let commands = vec!["true".to_string(), "false".to_string()];

        let result = run_checks(&commands).await.unwrap();

        assert!(!result.passed);
        assert_eq!(result.commands.len(), 2);
        assert!(result.commands[0].passed);
        assert!(!result.commands[1].passed);
    }

    #[tokio::test]
    async fn run_checks_with_empty_command_is_a_no_op() {
        let commands = vec![String::new()];

        let result = run_checks(&commands).await.unwrap();

        assert!(result.passed);
        assert_eq!(result.commands.len(), 1);
        assert_eq!(result.commands[0].command, "");
        assert!(result.commands[0].passed);
        assert!(result.commands[0].stdout.is_empty());
        assert!(result.commands[0].stderr.is_empty());
    }

    #[tokio::test]
    async fn run_checks_with_no_commands_passes() {
        let result = run_checks(&[]).await.unwrap();

        assert!(result.passed);
        assert!(result.commands.is_empty());
    }
}
