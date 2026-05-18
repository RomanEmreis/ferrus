use anyhow::Result;
use neva::prelude::*;
use tracing::{info, warn};

use crate::{
    config::Config,
    project,
    state::{
        machine::{TaskState, TransitionError},
        store,
    },
};

use super::{ensure_lease_owner, tool_err};

pub const DESCRIPTION: &str = "Reject the current submission with review notes. Writes notes to REVIEW.md and \
     transitions state Reviewing → Addressing (or Failed if the review cycle limit is \
     exhausted). The Executor's check retry counter is reset for the new cycle.";

pub const INPUT_SCHEMA: &str = r#"{
    "properties": {
        "notes": {
            "type": "string",
            "description": "Markdown-formatted review notes explaining what needs to change"
        }
    },
    "required": ["notes"]
}"#;

pub async fn handler_for_agent(agent_id: &str, notes: String) -> Result<String, Error> {
    run(agent_id, notes).await.map_err(tool_err)
}

async fn run(agent_id: &str, notes: String) -> Result<String> {
    let config = Config::load().await?;
    let mut state = store::read_state().await?;

    if state.state != TaskState::Reviewing {
        anyhow::bail!(
            "Cannot reject from state {:?}. Call /review_pending first.",
            state.state
        );
    }
    ensure_lease_owner(&state, agent_id)?;

    store::write_review(&notes).await?;

    match state.reject(config.limits.max_review_cycles) {
        Ok(()) => {
            store::write_state(&state).await?;
            project::record_current_task_status_best_effort("addressing").await;
            project::record_runtime_event_best_effort(
                None,
                "rejected",
                serde_json::json!({
                    "review_cycles": state.review_cycles,
                    "max_review_cycles": config.limits.max_review_cycles,
                    "notes_bytes": notes.len(),
                }),
            )
            .await;
            info!(
                review_cycles = state.review_cycles,
                "Submission rejected, state → Addressing"
            );
            Ok(format!(
                "Submission rejected (cycle {}/{}).\n\n**Review notes written.** \
                 State: Addressing. The Executor should call /wait_for_task to see the notes \
                 and /check after addressing them.",
                state.review_cycles, config.limits.max_review_cycles,
            ))
        }
        Err(TransitionError::ReviewLimitExceeded { cycles }) => {
            store::write_state(&state).await?;
            project::record_current_task_status_best_effort("failed").await;
            project::record_runtime_event_best_effort(
                None,
                "review_limit_exceeded",
                serde_json::json!({
                    "review_cycles": cycles,
                    "max_review_cycles": config.limits.max_review_cycles,
                    "notes_bytes": notes.len(),
                }),
            )
            .await;
            warn!(cycles, "Review cycle limit reached, state → Failed");
            Ok(format!(
                "Review cycle limit reached ({cycles}/{}).\n\nState is now Failed. \
                 A human must call /reset to recover.",
                config.limits.max_review_cycles,
            ))
        }
        Err(e) => anyhow::bail!(e),
    }
}
