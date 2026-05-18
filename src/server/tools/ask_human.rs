use anyhow::Result;
use tracing::info;

use crate::{
    config::Config,
    project::RuntimeTaskContext,
    state::{machine::StateData, store},
};

use super::{
    ensure_can_ask_human_or_reclaim, runtime_task_context_for_agent_best_effort, tool_err,
};

pub const DESCRIPTION: &str = "Ask the human a question. \
     Writes the question to QUESTION.md, transitions state to AwaitingHuman, \
     and returns immediately. You MUST call /wait_for_answer immediately after \
     to block until the human responds — do not call any other tools in between. \
     Can be called from Executing, Addressing, Consultation, or Reviewing state.";

pub const INPUT_SCHEMA: &str = r#"{
    "properties": {
        "question": {
            "type": "string",
            "description": "The question to ask the human"
        }
    },
    "required": ["question"]
}"#;

pub async fn handler_for_agent(
    agent_id: &str,
    question: String,
) -> Result<String, neva::prelude::Error> {
    run(agent_id, question).await.map_err(tool_err)
}

async fn run(agent_id: &str, question: String) -> Result<String> {
    let config = Config::load().await?;
    let mut state = store::read_state().await?;
    ensure_can_ask_human_or_reclaim(&mut state, agent_id, config.lease.ttl_secs).await?;
    let runtime_context = runtime_task_context_for_agent_best_effort(agent_id).await;
    let paused = state.ask_human()?;
    state.awaiting_human_by = Some(agent_id.to_string());
    write_question(&state, runtime_context.as_ref(), &question).await?;
    store::write_state(&state).await?;

    info!(paused = ?paused, "State → AwaitingHuman");
    Ok(format!(
        "Your question has been written to `.ferrus/QUESTION.md`.\n\
         State is now AwaitingHuman (paused from {paused:?}).\n\
         Call /wait_for_answer immediately to block until the human responds.\n\
         Do NOT call any other tools while waiting."
    ))
}

async fn write_question(
    state: &StateData,
    context: Option<&RuntimeTaskContext>,
    question: &str,
) -> Result<()> {
    if let Some(context) = context {
        store::write_question_for_run_dir(&context.run_dir, question).await?;
        if state.active_task_id.as_deref() == Some(context.task_id.as_str()) {
            store::write_question(question).await?;
        }
        return Ok(());
    }
    store::write_question(question).await
}
