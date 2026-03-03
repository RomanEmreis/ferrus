use anyhow::Result;
use neva::prelude::*;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::info;

use crate::{
    config::Config,
    state::{machine::TaskState, store},
};

use super::tool_err;

pub const DESCRIPTION: &str =
    "Block until the Executor submits work for review, then return the full submission \
     context. Polls STATE.json until the state is Reviewing, then returns the task, \
     submission notes, any check feedback, and any prior review notes. \
     Times out after `wait_timeout_secs` (see ferrus.toml). \
     Returns immediately if a submission is already pending — safe to call on restart.";

pub async fn handler() -> Result<String, Error> {
    run().await.map_err(tool_err)
}

async fn run() -> Result<String> {
    let config = Config::load().await?;
    let timeout = Duration::from_secs(config.limits.wait_timeout_secs);
    let start = Instant::now();

    loop {
        let state = store::read_state().await?;

        if state.state == TaskState::Reviewing {
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

            info!("Supervisor woke up: submission ready for review");
            return Ok(response);
        }

        if start.elapsed() >= timeout {
            anyhow::bail!(
                "Timed out after {}s waiting for a submission. Current state: {:?}.",
                config.limits.wait_timeout_secs,
                state.state
            );
        }

        sleep(Duration::from_millis(500)).await;
    }
}
