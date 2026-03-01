use anyhow::Result;
use neva::prelude::*;
use tracing::info;

use crate::state::{machine::TaskState, store};

use super::tool_err;

pub const DESCRIPTION: &str =
    "Human escape hatch: reset a Failed task back to Idle. Clears FEEDBACK.md and \
     REVIEW.md. Only valid in state Failed.";

pub async fn handler() -> Result<String, Error> {
    run().await.map_err(tool_err)
}

async fn run() -> Result<String> {
    let mut state = store::read_state().await?;

    if state.state != TaskState::Failed {
        anyhow::bail!(
            "Cannot reset from state {:?}. Reset is only available in the Failed state.",
            state.state
        );
    }

    state.reset()?;
    store::write_state(&state).await?;
    store::clear_feedback().await?;
    store::clear_review().await?;

    info!("State reset, Idle");
    Ok("State reset to Idle. FEEDBACK.md and REVIEW.md cleared. Ready for a new task.".to_string())
}
