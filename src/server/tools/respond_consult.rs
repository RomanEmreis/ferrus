use anyhow::Result;
use neva::prelude::*;
use tracing::info;

use crate::state::{machine::TaskState, store};

use super::tool_err;

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

pub async fn handler(response: String) -> Result<String, Error> {
    run(response).await.map_err(tool_err)
}

async fn run(response: String) -> Result<String> {
    if response.trim().is_empty() {
        anyhow::bail!("Consultation response cannot be empty.");
    }

    let state = store::read_state().await?;

    if state.state != TaskState::Consultation {
        anyhow::bail!(
            "Cannot respond to consultation from state {:?}. /respond_consult is only valid in Consultation state.",
            state.state
        );
    }

    store::write_consult_response(&response).await?;
    info!("Consultation response recorded");
    Ok("Consultation response recorded in `.ferrus/CONSULT_RESPONSE.md`.".to_string())
}
