use anyhow::Result;
use neva::prelude::*;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::info;

use crate::{
    config::Config,
    state::{machine::TaskState, store},
};

use super::tool_err;

pub const DESCRIPTION: &str =
    "Block until CONSULT_RESPONSE.md exists, then restore the pre-consult state and \
     return the consultant's response text. Must only be called while state is Consultation.";

pub async fn handler() -> Result<String, Error> {
    run().await.map_err(tool_err)
}

async fn run() -> Result<String> {
    let config = Config::load().await?;
    let timeout = Duration::from_secs(config.limits.wait_timeout_secs);
    let start = Instant::now();

    let state = store::read_state().await?;
    if state.state != TaskState::Consultation {
        anyhow::bail!(
            "Cannot wait for consultation from state {:?}. Call /consult first.",
            state.state
        );
    }

    loop {
        match store::read_consult_response().await {
            Ok(response) if !response.trim().is_empty() => {
                let mut state = store::read_state().await?;
                let resumed = state.finish_consult()?;
                if let Some(agent_id) = state.claimed_by.clone() {
                    store::claim_state(&agent_id, config.lease.ttl_secs, &mut state).await?;
                } else {
                    store::write_state(&state).await?;
                }
                store::clear_consult_response().await?;
                store::clear_consult_request().await?;

                let response = response.trim().to_string();
                info!(resumed = ?resumed, "Consultation answered; state restored");
                return Ok(response);
            }
            _ => {}
        }

        if start.elapsed() >= timeout {
            anyhow::bail!(
                "Timed out waiting for CONSULT_RESPONSE.md. Call /wait_for_consult again to keep waiting."
            );
        }

        sleep(Duration::from_millis(500)).await;
    }
}
