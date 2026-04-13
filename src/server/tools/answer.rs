use anyhow::Result;
use neva::prelude::*;
use tracing::info;

use crate::state::{machine::TaskState, store};

use super::tool_err;

pub const DESCRIPTION: &str = "Provide a response to a pending human question when the state is AwaitingHuman. \
     Writes the response to ANSWER.md and restores the previous state so the agent can continue.";

pub const INPUT_SCHEMA: &str = r#"{
    "properties": {
        "response": {
            "type": "string",
            "description": "The response to the question written in QUESTION.md"
        }
    },
    "required": ["response"]
}"#;

pub async fn handler(response: String) -> Result<String, Error> {
    run(response).await.map_err(tool_err)
}

async fn run(response: String) -> Result<String> {
    let mut state = store::read_state().await?;

    if state.state != TaskState::AwaitingHuman {
        anyhow::bail!(
            "Cannot answer from state {:?}. /answer is only valid in AwaitingHuman state.",
            state.state
        );
    }

    let resumed = state.answer()?;
    store::write_answer(&response).await?;
    store::write_state(&state).await?;

    info!(resumed = ?resumed, "State → {resumed:?} (resumed from AwaitingHuman)");
    Ok(format!(
        "Response recorded in `.ferrus/ANSWER.md`. State restored to {resumed:?}. \
         The agent can read the answer and continue."
    ))
}
