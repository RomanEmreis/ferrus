use anyhow::Result;
use neva::prelude::*;
use tracing::{info, warn};

use crate::{
    checks::runner,
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
        let feedback = format!(
            "# Check Failures\n\nRetry {}/{}\n\n{}",
            state.check_retries + 1,
            config.limits.max_check_retries,
            result.output,
        );

        match state.check_failed(result.output.clone(), config.limits.max_check_retries) {
            Ok(()) => {
                store::write_feedback(&feedback).await?;
                store::write_state(&state).await?;
                warn!(
                    retries = state.check_retries,
                    "Checks failed, state → Addressing"
                );
                Ok(format!(
                    "Checks failed (retry {}/{}).\n\n{feedback}\n\nFix the issues and call \
                     /check again.",
                    state.check_retries, config.limits.max_check_retries,
                ))
            }
            Err(TransitionError::CheckLimitExceeded { retries }) => {
                store::write_feedback(&feedback).await?;
                store::write_state(&state).await?;
                warn!(retries, "Check retry limit reached, state → Failed");
                Ok(format!(
                    "Check retry limit reached ({retries}/{}).\n\n{feedback}\n\n\
                     State is now Failed. A human must call /reset to recover.",
                    config.limits.max_check_retries,
                ))
            }
            Err(e) => anyhow::bail!(e),
        }
    }
}
