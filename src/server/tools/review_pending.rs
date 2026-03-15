use anyhow::Result;
use neva::prelude::*;
use tracing::info;

use crate::{
    config::Config,
    state::{machine::TaskState, store},
};

use super::tool_err;

pub const DESCRIPTION: &str =
    "Retrieve the pending submission for review. Returns the task description, \
     the Executor's submission notes (summary, verification steps, known limitations), \
     and any prior feedback or review notes. Only valid in state Reviewing.";

pub async fn handler() -> Result<String, Error> {
    run().await.map_err(tool_err)
}

async fn run() -> Result<String> {
    let config = Config::load().await?;
    let state = store::read_state().await?;

    if state.state != TaskState::Reviewing {
        anyhow::bail!(
            "No submission pending review. Current state: {:?}. \
             Wait for the Executor to call /submit.",
            state.state
        );
    }

    let task = store::read_task().await?;
    let submission = store::read_submission().await?;
    let feedback = store::read_feedback().await?;
    let review = store::read_review().await?;

    let mut response = format!("## Task\n\n{task}\n");

    if !submission.trim().is_empty() {
        response.push_str("\n## Submission Notes\n\n");
        response.push_str(&submission);
    }

    if !feedback.trim().is_empty() {
        response.push_str("\n## Last Check Output\n\n");
        response.push_str(&feedback);
    }

    if !review.trim().is_empty() {
        response.push_str("\n## Previous Review Notes\n\n");
        response.push_str(&review);
    }

    response.push_str(&format!(
        "\n---\nReview cycles used: {}/{}  \nCheck retries used: {}/{}",
        state.review_cycles,
        config.limits.max_review_cycles,
        state.check_retries,
        config.limits.max_check_retries,
    ));

    info!("Supervisor fetched pending review");
    Ok(response)
}
