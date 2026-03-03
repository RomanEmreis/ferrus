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

    Ok(CheckResult { passed, commands: results })
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
