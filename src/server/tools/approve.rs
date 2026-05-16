use anyhow::Result;
use neva::prelude::*;
use tracing::info;

use crate::{
    project, specs,
    state::{machine::TaskState, store},
};

use super::tool_err;

pub const DESCRIPTION: &str = "Approve the current submission. Transitions state Reviewing → Complete. \
     Must be called after /review_pending.";

pub async fn handler() -> Result<String, Error> {
    run().await.map_err(tool_err)
}

async fn run() -> Result<String> {
    let mut state = store::read_state().await?;

    if state.state != TaskState::Reviewing {
        anyhow::bail!(
            "Cannot approve from state {:?}. Call /review_pending first.",
            state.state
        );
    }

    specs::complete_task_milestone_and_advance(&mut state).await?;
    state.approve()?;
    store::write_state(&state).await?;
    project::record_current_task_status_best_effort("complete").await;
    project::record_runtime_event_best_effort(None, "approved", serde_json::json!({})).await;

    info!("Task approved, state → Complete");
    Ok("Task approved. State: Complete. Well done!".to_string())
}
