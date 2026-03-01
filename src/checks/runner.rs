use anyhow::{Context, Result};
use tokio::process::Command;

pub struct CheckResult {
    pub passed: bool,
    /// Markdown-formatted aggregated output of all failing commands.
    pub output: String,
}

/// Run every configured check command in order. Stops accumulating on the first
/// failure but still returns after collecting that command's output.
pub async fn run_checks(commands: &[String]) -> Result<CheckResult> {
    let mut aggregated = String::new();
    let mut passed = true;

    for cmd in commands {
        let result = run_command(cmd)
            .await
            .with_context(|| format!("Failed to spawn command: {cmd}"))?;
        if !result.passed {
            passed = false;
            aggregated.push_str(&format!("## `{cmd}`\n\n"));
            aggregated.push_str(&result.output);
            aggregated.push('\n');
        }
    }

    Ok(CheckResult {
        passed,
        output: aggregated,
    })
}

async fn run_command(cmd: &str) -> Result<CheckResult> {
    // Simple whitespace split — quoted arguments are not supported in ferrus.toml.
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    let (program, args) = match parts.split_first() {
        Some(pair) => pair,
        None => return Ok(CheckResult { passed: true, output: String::new() }),
    };

    let output = Command::new(program)
        .args(args)
        .output()
        .await
        .with_context(|| format!("Failed to run `{program}`"))?;

    let passed = output.status.success();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let combined = format_output(&stdout, &stderr);

    Ok(CheckResult { passed, output: combined })
}

fn format_output(stdout: &str, stderr: &str) -> String {
    match (stdout.trim().is_empty(), stderr.trim().is_empty()) {
        (true, true) => String::new(),
        (true, false) => format!("```\n{stderr}```\n"),
        (false, true) => format!("```\n{stdout}```\n"),
        (false, false) => format!("**stdout:**\n```\n{stdout}```\n\n**stderr:**\n```\n{stderr}```\n"),
    }
}
