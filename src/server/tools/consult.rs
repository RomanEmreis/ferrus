use anyhow::Result;
use neva::prelude::*;
use tracing::info;

use crate::state::{machine::TaskState, store};

use super::tool_err;

pub const DESCRIPTION: &str = "Ask the configured Supervisor for a consultation. \
     Writes CONSULT_REQUEST.md, transitions state to Consultation, clears any stale \
     CONSULT_RESPONSE.md, and returns immediately. HQ will spawn the consultant Supervisor. \
     After calling this tool, call /wait_for_consult to block until the answer is ready.";

pub const INPUT_SCHEMA: &str = r#"{
    "properties": {
        "question": {
            "type": "string",
            "description": "The executor's consultation request for the supervisor"
        }
    },
    "required": ["question"]
}"#;

pub async fn handler(mut ctx: Context, question: String) -> Result<String, Error> {
    run(&mut ctx, question).await.map_err(tool_err)
}

async fn run(_ctx: &mut Context, question: String) -> Result<String> {
    let mut state = store::read_state().await?;
    if !matches!(
        state.state,
        TaskState::Executing | TaskState::Addressing | TaskState::Checking
    ) {
        anyhow::bail!(
            "Cannot consult from state {:?}. Consultation is only available while executing work.",
            state.state
        );
    }

    store::write_consult_request(&question).await?;
    store::clear_consult_response().await?;
    let paused = state.consult()?;
    store::write_state(&state).await?;

    info!(paused = ?paused, "State → Consultation");
    Ok(format!(
        "Consultation requested in `.ferrus/CONSULT_REQUEST.md`.\n\
         State is now Consultation (paused from {paused:?}).\n\
         HQ should spawn the configured Supervisor in consultation mode.\n\
         Call /wait_for_consult to block until the response is ready.",
    ))
}
