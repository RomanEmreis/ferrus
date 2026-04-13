use anyhow::Result;
use fs2::FileExt;
use neva::prelude::*;
use serde_json::json;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::info;

use crate::{
    config::Config,
    state::{machine::TaskState, store},
};

use super::tool_err;

pub const DESCRIPTION: &str = "Block until the Executor submits work for review, then atomically claim the review and \
     return the full submission context. \
     Returns a JSON object: {\"status\":\"claimed\", \"claimed_by\":\"...\", \"lease_until\":\"...\", \
     \"state\":\"Reviewing\", \"task\":\"...\", \"submission\":\"...\", \"review\":\"...\"} \
     when a submission is ready, or {\"status\":\"timeout\", \"state\":\"...\"} on timeout. \
     Each call waits up to `wait_timeout_secs` (see ferrus.toml), then returns timeout so the \
     agent can poll again. \
     Returns immediately if a submission is already pending — safe to call on restart.";

pub async fn handler(agent_id: &str) -> Result<String, Error> {
    run(agent_id).await.map_err(tool_err)
}

async fn run(agent_id: &str) -> Result<String> {
    let config = Config::load().await?;
    let timeout = Duration::from_secs(config.limits.wait_timeout_secs);
    let ttl_secs = config.lease.ttl_secs;
    let start = Instant::now();

    loop {
        let (claimed, _) = {
            let lock_file = store::open_lock_file()?;
            let lock_file = tokio::task::spawn_blocking(move || -> Result<std::fs::File> {
                lock_file.lock_exclusive().map_err(anyhow::Error::from)?;
                Ok(lock_file)
            })
            .await??;

            let mut state = store::read_state().await?;

            let claimable = state.state == TaskState::Reviewing;
            let claimed = if claimable && !state.is_claimed() {
                store::claim_state(agent_id, ttl_secs, &mut state).await?;
                true
            } else {
                claimable && state.is_claimed_by(agent_id)
            };

            drop(lock_file);
            (claimed, state)
        };

        if claimed {
            let task = store::read_task().await?;
            let submission = store::read_submission().await?;
            let review = store::read_review().await?;
            let state = store::read_state().await?;

            info!(agent_id, "Supervisor claimed review");
            let response = json!({
                "status": "claimed",
                "claimed_by": state.claimed_by,
                "lease_until": state.lease_until,
                "state": format!("{:?}", state.state),
                "task": task,
                "submission": submission,
                "review": review,
                "review_cycles_used": state.review_cycles,
                "check_retries_used": state.check_retries,
            });
            return Ok(response.to_string());
        }

        if start.elapsed() >= timeout {
            let state = store::read_state().await?;
            info!("wait_for_review timed out, state: {:?}", state.state);
            let response = json!({
                "status": "timeout",
                "state": format!("{:?}", state.state),
            });
            return Ok(response.to_string());
        }

        sleep(Duration::from_millis(500)).await;
    }
}
