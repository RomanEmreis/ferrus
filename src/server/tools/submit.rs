use anyhow::Result;
use neva::prelude::*;
use tracing::info;

use crate::state::{machine::TaskState, store};

use super::tool_err;

pub const DESCRIPTION: &str =
    "Signal that checks have passed and the work is ready for Supervisor review. \
     Transitions state Checking → Reviewing. Must be called after /check returns \
     a passing result.";

pub async fn handler() -> Result<String, Error> {
    run().await.map_err(tool_err)
}

async fn run() -> Result<String> {
    let mut state = store::read_state().await?;

    if state.state != TaskState::Checking {
        anyhow::bail!(
            "Cannot submit from state {:?}. Call /check first and ensure all checks pass.",
            state.state
        );
    }

    state.submit()?;
    store::write_state(&state).await?;

    info!("Work submitted for review, state → Reviewing");
    Ok("Submitted for review. State: Reviewing. The Supervisor can now call /review_pending."
        .to_string())
}
