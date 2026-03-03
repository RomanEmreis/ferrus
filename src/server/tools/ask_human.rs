use anyhow::Result;
use neva::prelude::*;
use tracing::{info, warn};

use crate::state::store;

use super::tool_err;

pub const DESCRIPTION: &str =
    "Ask the human a question and wait for a response. \
     Uses MCP elicitation when supported by the client (response is returned inline). \
     Falls back to writing the question to QUESTION.md and transitioning to AwaitingHuman \
     state when elicitation is unavailable — the human must then call /answer to resume. \
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

async fn run(ctx: &mut Context, question: String) -> Result<String> {
    // Attempt elicitation — works when the MCP client supports it.
    let params: ElicitRequestParams = ElicitRequestParams::form(&question)
        .with_required("response", Schema::String(StringSchema::default()))
        .into();

    match ctx.elicit(params).await {
        Ok(result) => {
            return match result.action {
                ElicitationAction::Accept => {
                    let response = result
                        .content
                        .as_ref()
                        .and_then(|c| c.get("response"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    info!("Human answered via elicitation");
                    Ok(response)
                }
                ElicitationAction::Cancel | ElicitationAction::Decline => {
                    info!("Human declined elicitation");
                    Ok("The human declined to answer.".to_string())
                }
            };
        }
        Err(_) => {
            warn!("Elicitation unavailable — falling back to AwaitingHuman state");
        }
    }

    // Fallback: persist the question and pause state.
    let mut state = store::read_state().await?;
    let paused = state.ask_human()?;
    store::write_question(&question).await?;
    store::write_state(&state).await?;

    info!(paused = ?paused, "State → AwaitingHuman");
    Ok(format!(
        "Elicitation is not supported by this client.\n\
         Question written to `.ferrus/QUESTION.md`. State is now AwaitingHuman \
         (paused from {paused:?}).\n\
         A human must call `/answer` with a response to resume."
    ))
}
