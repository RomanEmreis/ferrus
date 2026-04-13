use anyhow::Result;
use neva::prelude::*;
use tracing::info;

use crate::{
    config::Config,
    state::{machine::TaskState, machine::TransitionError, store},
};

use super::{
    check_gate::{self, CheckGateResult},
    tool_err,
};

pub const DESCRIPTION: &str = "\
Run the final check gate and, if it passes, submit work for Supervisor review. \
Can be called from Executing or Addressing. \
On pass: state → Reviewing. On fail: stay in the current work state (or state \
→ Failed if the retry limit is exhausted).

The `content` parameter must be a Markdown document with the following sections:

## Summary
Brief description of what was changed and why.

## How to verify manually
Step-by-step instructions for the Supervisor to spot-check the work.

## Known limitations
Anything deliberately left out, edge cases not handled, or follow-up work needed. \
Omit this section if there are none.";

pub const INPUT_SCHEMA: &str = r#"{
    "properties": {
        "content": {
            "type": "string",
            "description": "Submission notes in Markdown (summary, how to verify, known limitations)"
        }
    },
    "required": ["content"]
}"#;

pub async fn handler(content: String) -> Result<String, Error> {
    run(content).await.map_err(tool_err)
}

async fn run(content: String) -> Result<String> {
    let config = Config::load().await?;
    let mut state = store::read_state().await?;

    if !matches!(state.state, TaskState::Executing | TaskState::Addressing) {
        anyhow::bail!(
            "Cannot submit from state {:?}. Submit is only valid from Executing or Addressing after the implementation is ready.",
            state.state
        );
    }

    if config.checks.commands.is_empty() {
        info!("No check commands configured; treating final check gate as pass");
        state.check_passed()?;
        state.submit()?;
        store::write_submission(&content).await?;
        store::write_state(&state).await?;

        return Ok(
            "Submitted for review. Warning: no check commands are configured in ferrus.toml, so the final check gate was treated as a pass. State: Reviewing."
                .to_string(),
        );
    }

    info!("Running final check gate before review submission");
    match check_gate::run(&config, state.check_retries + 1).await? {
        CheckGateResult::Passed => {
            state.check_passed()?;
            state.submit()?;
            store::write_submission(&content).await?;
            store::write_state(&state).await?;

            info!("Work submitted for review, state → Reviewing");
            Ok(
                "Submitted for review. State: Reviewing. The Supervisor can now call /review_pending."
                    .to_string(),
            )
        }
        CheckGateResult::Failed(failure) => {
            match state.check_failed(failure.failure_reason, config.limits.max_check_retries) {
                Ok(()) => {
                    store::write_state(&state).await?;
                    Ok(format!(
                        "Final review gate failed during /submit (retry {}/{}).\n\n{}\n\nState remains {:?}. Fix the issues and run /check or /submit again.",
                        state.check_retries,
                        config.limits.max_check_retries,
                        failure.report,
                        state.state,
                    ))
                }
                Err(TransitionError::CheckLimitExceeded { retries }) => {
                    store::write_state(&state).await?;
                    Ok(format!(
                        "Final review gate failed during /submit and hit the retry limit ({retries}/{}).\n\n{}\n\nState is now Failed. A human must call /reset to recover.",
                        config.limits.max_check_retries, failure.report,
                    ))
                }
                Err(e) => anyhow::bail!(e),
            }
        }
    }
}
