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

use super::tool_err;

pub const DESCRIPTION: &str = "Reject the current submission with review notes. Writes notes to REVIEW.md and \
     transitions state Reviewing → Addressing (or Failed if the review cycle limit is \
     exhausted). The Executor's check retry counter is reset for the new cycle.";

pub const INPUT_SCHEMA: &str = r#"{
    "properties": {
        "notes": {
            "type": "string",
            "description": "Markdown-formatted review notes explaining what needs to change"
        }
    },
    "required": ["notes"]
}"#;

pub async fn handler(notes: String) -> Result<String, Error> {
    run(notes).await.map_err(tool_err)
}

async fn run(notes: String) -> Result<String> {
    let config = Config::load().await?;
    let mut state = store::read_state().await?;

    if state.state != TaskState::Reviewing {
        anyhow::bail!(
            "Cannot reject from state {:?}. Call /review_pending first.",
            state.state
        );
    }

    store::write_review(&notes).await?;

    match state.reject(config.limits.max_review_cycles) {
        Ok(()) => {
            store::write_state(&state).await?;
            info!(
                review_cycles = state.review_cycles,
                "Submission rejected, state → Addressing"
            );
            Ok(format!(
                "Submission rejected (cycle {}/{}).\n\n**Review notes written.** \
                 State: Addressing. The Executor should call /wait_for_task to see the notes \
                 and /check after addressing them.",
                state.review_cycles, config.limits.max_review_cycles,
            ))
        }
        Err(TransitionError::ReviewLimitExceeded { cycles }) => {
            store::write_state(&state).await?;
            warn!(cycles, "Review cycle limit reached, state → Failed");
            Ok(format!(
                "Review cycle limit reached ({cycles}/{}).\n\nState is now Failed. \
                 A human must call /reset to recover.",
                config.limits.max_review_cycles,
            ))
        }
        Err(e) => anyhow::bail!(e),
    }
}
