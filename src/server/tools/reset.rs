use anyhow::Result;
use neva::prelude::*;
use tracing::info;

use crate::{
    project,
    state::{machine::TaskState, store},
};

use super::tool_err;

pub const DESCRIPTION: &str = "Human escape hatch: reset a Failed task back to Idle. Clears REVIEW.md, \
     SUBMISSION.md, and consultation files. Only valid in state Failed.";

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

    let active_task = state
        .active_task_id
        .clone()
        .zip(state.active_task_path.clone());

    store::clear_review_for_state(&state).await?;
    store::clear_submission_for_state(&state).await?;
    store::clear_consult_request().await?;
    store::clear_consult_response().await?;
    state.reset()?;
    store::write_state(&state).await?;

    if let Some((task_id, task_path)) = active_task {
        project::record_task_status_best_effort(&task_id, &task_path, "reset").await;
    }
    project::record_current_task_status_best_effort("idle").await;
    project::record_runtime_event_best_effort(None, "reset", serde_json::json!({})).await;

    info!("State reset, Idle");
    Ok("State reset to Idle. REVIEW.md, SUBMISSION.md, and consultation files cleared. Ready for a new task.".to_string())
}
