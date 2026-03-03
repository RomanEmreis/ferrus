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
    "Block until a task is ready to work on, then return its full context. \
     Polls STATE.json until the state is Executing or Addressing, then returns \
     the task description, any check feedback, and any Supervisor review notes. \
     Times out after `wait_timeout_secs` (see ferrus.toml). \
     Call this at startup and after each /submit to form an autonomous loop.";

pub async fn handler() -> Result<String, Error> {
    run().await.map_err(tool_err)
}

async fn run() -> Result<String> {
    let config = Config::load().await?;
    let timeout = Duration::from_secs(config.limits.wait_timeout_secs);
    let start = Instant::now();

    loop {
        let state = store::read_state().await?;

        match state.state {
            TaskState::Executing => {
                let task = store::read_task().await?;
                info!("Executor woke up: task ready (Executing)");
                return Ok(format!("## Task\n\n{task}"));
            }
            TaskState::Addressing => {
                let task = store::read_task().await?;
                let feedback = store::read_feedback().await?;
                let review = store::read_review().await?;

                let mut response = format!("## Task\n\n{task}\n\n## Check Failures\n\n");
                if feedback.trim().is_empty() {
                    response.push_str("_(no check output)_\n");
                } else {
                    response.push_str(&feedback);
                }
                if !review.trim().is_empty() {
                    response.push_str("\n## Supervisor Review Notes\n\n");
                    response.push_str(&review);
                }

                info!("Executor woke up: addressing required");
                return Ok(response);
            }
            _ => {}
        }

        if start.elapsed() >= timeout {
            anyhow::bail!(
                "Timed out after {}s waiting for a task. Current state: {:?}.",
                config.limits.wait_timeout_secs,
                state.state
            );
        }

        sleep(Duration::from_millis(500)).await;
    }
}
