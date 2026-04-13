use anyhow::Result;
use neva::prelude::*;
use tracing::info;

use crate::state::{machine::TaskState, store};

use super::tool_err;

pub const DESCRIPTION: &str = "Create a new task for the Executor. Transitions state Idle → Executing and writes \
     the task description to TASK.md. Must be called from state Idle.";

pub const INPUT_SCHEMA: &str = r#"{
    "properties": {
        "description": {
            "type": "string",
            "description": "Full task description in Markdown"
        }
    },
    "required": ["description"]
}"#;

pub async fn handler(description: String) -> Result<String, Error> {
    run(description).await.map_err(tool_err)
}

async fn run(description: String) -> Result<String> {
    let mut state = store::read_state().await?;

    if state.state != TaskState::Idle {
        anyhow::bail!(
            "Cannot create task: current state is {:?}. \
             The executor must complete or reset the current task first.",
            state.state
        );
    }

    state.create_task()?;
    store::write_task(&description).await?;
    store::clear_submission().await?;
    store::clear_consult_request().await?;
    store::clear_consult_response().await?;
    store::write_state(&state).await?;

    info!("Task created, state → Executing");
    Ok("Task created. State: Executing. The Executor can now call /wait_for_task.".to_string())
}
