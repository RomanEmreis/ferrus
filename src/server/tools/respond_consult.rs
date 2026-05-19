use anyhow::Result;
use neva::prelude::*;
use tracing::info;

use crate::{
    project::RuntimeTaskContext,
    state::{machine::TaskState, store},
};

use super::{runtime_task_context_for_agent_best_effort, tool_err};

pub const DESCRIPTION: &str = "Record the Supervisor's consultation response. \
     Writes the response to CONSULT_RESPONSE.md. Must only be called while state is Consultation.";

pub const INPUT_SCHEMA: &str = r#"{
    "properties": {
        "response": {
            "type": "string",
            "description": "The Supervisor's consultation response"
        }
    },
    "required": ["response"]
}"#;

pub async fn handler_for_agent(agent_id: &str, response: String) -> Result<String, Error> {
    run(Some(agent_id), response).await.map_err(tool_err)
}

async fn run(agent_id: Option<&str>, response: String) -> Result<String> {
    if response.trim().is_empty() {
        anyhow::bail!("Consultation response cannot be empty.");
    }

    let state = store::read_state().await?;
    let runtime_context = match agent_id {
        Some(agent_id) => runtime_task_context_for_agent_best_effort(agent_id).await,
        None => None,
    };

    if state.state != TaskState::Consultation
        && !matches!(
            runtime_context
                .as_ref()
                .map(|context| context.status.as_str()),
            Some("consultation")
        )
    {
        anyhow::bail!(
            "Cannot respond to consultation from state {:?}. /respond_consult is only valid in Consultation state.",
            state.state
        );
    }

    write_consult_response(&state, runtime_context.as_ref(), &response).await?;
    info!("Consultation response recorded");
    Ok("Consultation response recorded in `.ferrus/CONSULT_RESPONSE.md`.".to_string())
}

async fn write_consult_response(
    state: &crate::state::machine::StateData,
    context: Option<&RuntimeTaskContext>,
    response: &str,
) -> Result<()> {
    if let Some(context) = context {
        store::write_consult_response_for_run_dir(&context.run_dir, response).await?;
        if state.active_task_id.as_deref() == Some(context.task_id.as_str()) {
            store::write_consult_response(response).await?;
        }
        return Ok(());
    }
    store::write_consult_response(response).await
}
