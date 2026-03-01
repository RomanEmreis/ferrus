use anyhow::Result;
use neva::prelude::*;
use tracing::info;

use crate::state::{machine::TaskState, store};

use super::tool_err;

#[tool(descr = "Poll for the next pending task. Returns the task description if state is \
                Executing or Addressing, otherwise reports that no task is ready.")]
async fn next_task() -> Result<String, Error> {
    run().await.map_err(tool_err)
}

async fn run() -> Result<String> {
    let state = store::read_state().await?;

    match state.state {
        TaskState::Executing => {
            let task = store::read_task().await?;
            info!("Executor picked up task");
            Ok(format!("## Task\n\n{task}"))
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

            info!("Executor resuming addressing");
            Ok(response)
        }
        other => Ok(format!(
            "No task is pending. Current state: {other:?}. \
             Wait for the Supervisor to call /create_task."
        )),
    }
}
