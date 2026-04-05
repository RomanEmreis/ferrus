use anyhow::Result;
use neva::prelude::*;
use tracing::info;

use crate::state::store;

use super::tool_err;

pub const DESCRIPTION: &str = "Ask the human a question. \
     Writes the question to QUESTION.md, transitions state to AwaitingHuman, \
     and returns immediately. You MUST call /wait_for_answer immediately after \
     to block until the human responds — do not call any other tools in between. \
     Can be called from Executing, Addressing, Checking, or Reviewing state.";

pub const INPUT_SCHEMA: &str = r#"{
    "properties": {
        "question": {
            "type": "string",
            "description": "The question to ask the human"
        }
    },
    "required": ["question"]
}"#;

pub async fn handler(mut ctx: Context, question: String) -> Result<String, Error> {
    run(&mut ctx, question).await.map_err(tool_err)
}

async fn run(_ctx: &mut Context, question: String) -> Result<String> {
    let mut state = store::read_state().await?;
    let paused = state.ask_human()?;
    store::write_question(&question).await?;
    store::write_state(&state).await?;

    info!(paused = ?paused, "State → AwaitingHuman");
    Ok(format!(
        "Your question has been written to `.ferrus/QUESTION.md`.\n\
         State is now AwaitingHuman (paused from {paused:?}).\n\
         Call /wait_for_answer immediately to block until the human responds.\n\
         Do NOT call any other tools while waiting."
    ))
}
