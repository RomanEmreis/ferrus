use anyhow::Result;
use neva::prelude::*;
use tracing::{info, warn};

use crate::{
    config::Config,
    state::{
        machine::{TaskState, TransitionError},
        store,
    },
};

use super::{
    check_gate::{self, CheckGateResult},
    tool_err,
};

pub const DESCRIPTION: &str = "Run all configured checks (clippy, fmt, tests, etc.) against the current \
     codebase. Can be called from state Executing or Addressing. \
     On pass: stay in the current work state and clear check-failure metadata. \
     On fail: stay in the current work state (or state → Failed if the retry \
     limit is exhausted).";

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
    match check_gate::run(&config, state.check_retries + 1).await? {
        CheckGateResult::Passed => {
            state.check_passed()?;
            store::write_state(&state).await?;
            info!(state = ?state.state, "All checks passed; staying in current work state");
            Ok(format!(
                "All checks passed. State remains {:?}. Continue working or call /submit when the task is ready for review.",
                state.state
            ))
        }
        CheckGateResult::Failed(failure) => {
            match state.check_failed(failure.failure_reason, config.limits.max_check_retries) {
                Ok(()) => {
                    store::write_state(&state).await?;
                    warn!(
                        retries = state.check_retries,
                        state = ?state.state,
                        "Checks failed; staying in current work state"
                    );
                    Ok(format!(
                        "Checks failed (retry {}/{}).\n\n{}\n\nState remains {:?}. Fix the issues and call /check again.",
                        state.check_retries,
                        config.limits.max_check_retries,
                        failure.report,
                        state.state,
                    ))
                }
                Err(TransitionError::CheckLimitExceeded { retries }) => {
                    store::write_state(&state).await?;
                    warn!(retries, "Check retry limit reached, state → Failed");
                    Ok(format!(
                        "Check retry limit reached ({retries}/{}).\n\n{}\n\nState is now Failed. A human must call /reset to recover.",
                        config.limits.max_check_retries, failure.report,
                    ))
                }
                Err(e) => anyhow::bail!(e),
            }
        }
    }
}
