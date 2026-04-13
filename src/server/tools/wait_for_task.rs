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

pub const DESCRIPTION: &str = "Block until a task is ready to work on, then atomically claim it and return its full context. \
     Returns a JSON object: {\"status\":\"claimed\", \"claimed_by\":\"...\", \"lease_until\":\"...\", \
     \"state\":\"...\", \"task\":\"...\", \"review\":\"...\"} when a task is \
     claimed, or {\"status\":\"timeout\", \"state\":\"...\"} on timeout. \
     On timeout, inspect the state field — call wait_for_task again only if the state is \
     Executing or Addressing. \
     Each call waits up to `wait_timeout_secs` (see ferrus.toml), then returns timeout so the \
     agent can poll again. \
     Call this at the start of each Executor session; after a rejection, the next Executor \
     session should call it again to claim the Addressing work.";

pub async fn handler(agent_id: &str) -> Result<String, Error> {
    run(agent_id).await.map_err(tool_err)
}

async fn run(agent_id: &str) -> Result<String> {
    let config = Config::load().await?;
    let timeout = Duration::from_secs(config.limits.wait_timeout_secs);
    let ttl_secs = config.lease.ttl_secs;
    let start = Instant::now();

    loop {
        // Acquire exclusive advisory lock, read state, conditionally claim — all atomically.
        let (claimed, _) = {
            let lock_file = store::open_lock_file()?;
            // Blocking call: run off the async thread so we don't block the runtime.
            let lock_file = tokio::task::spawn_blocking(move || -> Result<std::fs::File> {
                lock_file.lock_exclusive().map_err(anyhow::Error::from)?;
                Ok(lock_file)
            })
            .await??;

            let mut state = store::read_state().await?;

            let claimable = matches!(state.state, TaskState::Executing | TaskState::Addressing);
            let claimed = if claimable && !state.is_claimed() {
                store::claim_state(agent_id, ttl_secs, &mut state).await?;
                true
            } else if claimable && state.is_claimed_by(agent_id) {
                // Idempotent re-entry: this agent already holds the lease.
                true
            } else {
                false
            };

            // Release lock by dropping lock_file.
            drop(lock_file);
            (claimed, state)
        };

        if claimed {
            let task = store::read_task().await?;
            let review = store::read_review().await?;

            // Re-read state to get the stamped lease_until.
            let state = store::read_state().await?;

            info!(agent_id, "Executor claimed task");
            let response = json!({
                "status": "claimed",
                "claimed_by": state.claimed_by,
                "lease_until": state.lease_until,
                "state": format!("{:?}", state.state),
                "task": task,
                "review": review,
            });
            return Ok(response.to_string());
        }

        if start.elapsed() >= timeout {
            let state = store::read_state().await?;
            info!("wait_for_task timed out, state: {:?}", state.state);
            let response = json!({
                "status": "timeout",
                "state": format!("{:?}", state.state),
            });
            return Ok(response.to_string());
        }

        sleep(Duration::from_millis(500)).await;
    }
}
