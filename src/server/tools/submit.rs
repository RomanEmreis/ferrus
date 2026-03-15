use anyhow::Result;
use neva::prelude::*;
use tracing::info;

use crate::state::{machine::TaskState, store};

use super::tool_err;

pub const DESCRIPTION: &str = "\
Signal that checks have passed and submit work for Supervisor review. \
Transitions state Checking → Reviewing. Must be called after /check returns \
a passing result.

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
    let mut state = store::read_state().await?;

    if state.state != TaskState::Checking {
        anyhow::bail!(
            "Cannot submit from state {:?}. Call /check first and ensure all checks pass.",
            state.state
        );
    }

    state.submit()?;
    store::write_submission(&content).await?;
    store::write_state(&state).await?;

    info!("Work submitted for review, state → Reviewing");
    Ok(
        "Submitted for review. State: Reviewing. The Supervisor can now call /review_pending."
            .to_string(),
    )
}
