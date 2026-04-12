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

    validate_consult_request(&question)?;

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

fn validate_consult_request(question: &str) -> Result<()> {
    let trimmed = question.trim();
    if trimmed.is_empty() {
        anyhow::bail!(
            "Consultation request cannot be empty. Read ferrus://consult_template and follow it exactly."
        );
    }

    let required_sections = [
        "## Problem",
        "## What I tried",
        "## Options (if any)",
        "## Question",
    ];

    for section in required_sections {
        if !trimmed.contains(section) {
            anyhow::bail!(
                "Consultation request must follow ferrus://consult_template exactly. Missing section: {section}"
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_consult_request;

    #[test]
    fn consult_request_requires_template_sections() {
        let err = validate_consult_request("Implementation complete, what now?")
            .expect_err("request without template should be rejected");
        let msg = err.to_string();
        assert!(msg.contains("ferrus://consult_template"));
        assert!(msg.contains("## Problem"));
    }

    #[test]
    fn consult_request_accepts_template_shape() {
        let request = "## Problem\n/check appears unavailable.\n\n## What I tried\nRetried once.\n\n## Options (if any)\n- Retry again\n\n## Question\nShould I keep retrying /check?\n";
        validate_consult_request(request).expect("template-shaped request should be accepted");
    }
}
