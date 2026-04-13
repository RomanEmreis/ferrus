use anyhow::Result;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    checks::runner::{self, CommandResult},
    config::Config,
    state::store,
};

pub(super) enum CheckGateResult {
    Passed,
    Failed(CheckFailure),
}

pub(super) struct CheckFailure {
    pub failure_reason: String,
    pub report: String,
}

pub(super) async fn run(config: &Config, attempt: u32) -> Result<CheckGateResult> {
    let result = runner::run_checks(&config.checks.commands).await?;
    if result.passed {
        return Ok(CheckGateResult::Passed);
    }

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let log_content = build_full_log(&result.commands);
    let log_path = store::write_check_log(attempt, ts, &log_content).await?;

    let failed_commands: Vec<&str> = result
        .commands
        .iter()
        .filter(|c| !c.passed)
        .map(|c| c.command.as_str())
        .collect();
    let failure_reason = format!("Commands failed: {}", failed_commands.join(", "));
    let report = build_report(
        &result.commands,
        config.limits.max_feedback_lines,
        &log_path,
    );

    Ok(CheckGateResult::Failed(CheckFailure {
        failure_reason,
        report,
    }))
}

fn build_full_log(commands: &[CommandResult]) -> String {
    let mut out = String::new();
    for cmd in commands {
        let status = if cmd.passed { "PASS" } else { "FAIL" };
        out.push_str(&format!("=== [{status}] {}\n\n", cmd.command));
        if !cmd.stdout.trim().is_empty() {
            out.push_str("--- stdout ---\n");
            out.push_str(&cmd.stdout);
            if !cmd.stdout.ends_with('\n') {
                out.push('\n');
            }
        }
        if !cmd.stderr.trim().is_empty() {
            out.push_str("--- stderr ---\n");
            out.push_str(&cmd.stderr);
            if !cmd.stderr.ends_with('\n') {
                out.push('\n');
            }
        }
        out.push('\n');
    }
    out
}

fn build_report(commands: &[CommandResult], max_lines: usize, log_path: &PathBuf) -> String {
    let failed: Vec<&CommandResult> = commands.iter().filter(|c| !c.passed).collect();

    let mut out = String::from("Checks failed.\n\nFailed commands:\n");
    for cmd in &failed {
        out.push_str(&format!("- `{}`\n", cmd.command));
    }
    out.push('\n');

    for cmd in &failed {
        out.push_str(&format!("`{}`\n", cmd.command));
        let combined = format!("{}{}", cmd.stdout, cmd.stderr);
        let total_lines = combined.lines().count();
        let tail = last_n_lines(&combined, max_lines);
        if total_lines > max_lines {
            out.push_str(&format!("(last {max_lines} of {total_lines} lines)\n"));
        }
        out.push_str("```\n");
        out.push_str(&tail);
        if !tail.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("```\n\n");
    }

    out.push_str(&format!("Full log: `{}`", log_path.display()));
    out
}

fn last_n_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}
