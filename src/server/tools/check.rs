use anyhow::Result;
use neva::prelude::*;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

use crate::{
    checks::runner::{self, CommandResult},
    config::Config,
    state::{
        machine::{TaskState, TransitionError},
        store,
    },
};

use super::tool_err;

pub const DESCRIPTION: &str =
    "Run all configured checks (clippy, fmt, tests, etc.) against the current \
     codebase. Can be called from state Executing or Addressing. \
     On pass: state → Checking. On fail: state → Addressing (or Failed if the \
     retry limit is exhausted).";

pub async fn handler() -> Result<String, Error> {
    run().await.map_err(tool_err)
}

async fn run() -> Result<String> {
    let config = Config::load().await?;
    let mut state = store::read_state().await?;

    match state.state {
        TaskState::Executing | TaskState::Addressing => {}
        TaskState::Checking => anyhow::bail!(
            "Checks already passed (state: Checking). Call /submit to submit your work for review."
        ),
        ref other => anyhow::bail!(
            "Cannot run checks from state {other:?}. \
             Checks are only valid in Executing or Addressing state."
        ),
    }

    info!("Running {} check(s)", config.checks.commands.len());
    let result = runner::run_checks(&config.checks.commands).await?;

    if result.passed {
        state.check_passed()?;
        store::clear_feedback().await?;
        store::write_state(&state).await?;
        info!("All checks passed, state → Checking");
        Ok("All checks passed. State: Checking. Call /submit when ready for review.".to_string())
    } else {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // attempt is the human-readable number (1-based) *before* incrementing the counter
        let attempt = state.check_retries + 1;

        let log_content = build_full_log(&result.commands);
        let log_path = store::write_check_log(attempt, ts, &log_content).await?;

        let failed_commands: Vec<&str> = result
            .commands
            .iter()
            .filter(|c| !c.passed)
            .map(|c| c.command.as_str())
            .collect();
        let failure_reason = format!("Commands failed: {}", failed_commands.join(", "));

        let summary = build_summary(
            &result.commands,
            attempt,
            config.limits.max_check_retries,
            config.limits.max_feedback_lines,
            &log_path,
        );

        match state.check_failed(failure_reason, config.limits.max_check_retries) {
            Ok(()) => {
                store::write_feedback(&summary).await?;
                store::write_state(&state).await?;
                warn!(
                    retries = state.check_retries,
                    "Checks failed, state → Addressing"
                );
                Ok(format!(
                    "Checks failed (retry {}/{}).\n\n{summary}\n\nFix the issues and call \
                     /check again.",
                    state.check_retries, config.limits.max_check_retries,
                ))
            }
            Err(TransitionError::CheckLimitExceeded { retries }) => {
                store::write_feedback(&summary).await?;
                store::write_state(&state).await?;
                warn!(retries, "Check retry limit reached, state → Failed");
                Ok(format!(
                    "Check retry limit reached ({retries}/{}).\n\n{summary}\n\n\
                     State is now Failed. A human must call /reset to recover.",
                    config.limits.max_check_retries,
                ))
            }
            Err(e) => anyhow::bail!(e),
        }
    }
}

/// Full log written to `.ferrus/logs/`: every command with its complete stdout + stderr.
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

/// Short FEEDBACK.md summary: failed command list, last N lines per command, log path.
fn build_summary(
    commands: &[CommandResult],
    attempt: u32,
    max_retries: u32,
    max_lines: usize,
    log_path: &Path,
) -> String {
    let failed: Vec<&CommandResult> = commands.iter().filter(|c| !c.passed).collect();

    let mut out = format!("# Check Failures — Attempt {attempt}/{max_retries}\n\n");

    out.push_str("## Failed\n\n");
    for cmd in &failed {
        out.push_str(&format!("- `{}`\n", cmd.command));
    }
    out.push('\n');

    for cmd in &failed {
        out.push_str(&format!("## `{}`\n\n", cmd.command));
        // stdout then stderr — cargo tools emit errors on stderr, which lands at the end
        let combined = format!("{}{}", cmd.stdout, cmd.stderr);
        let total_lines = combined.lines().count();
        let tail = last_n_lines(&combined, max_lines);
        if total_lines > max_lines {
            out.push_str(&format!(
                "*(last {max_lines} of {total_lines} lines — full output in log)*\n\n"
            ));
        }
        out.push_str("```\n");
        out.push_str(&tail);
        if !tail.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("```\n\n");
    }

    out.push_str(&format!("Full log: `{}`\n", log_path.display()));
    out
}

fn last_n_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}
