use anyhow::Result;
use neva::prelude::*;
use serde_json::json;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::info;

use crate::{
    config::Config,
    state::{machine::TaskState, store},
};

use super::{ensure_answer_waiter, tool_err};

pub const DESCRIPTION: &str = "Block until the human provides an answer to the question you asked via /ask_human. \
     Polls .ferrus/ANSWER.md until it has content, then restores the paused state and \
     returns the answer. \
     Returns {\"status\":\"answered\", \"answer\":\"...\", \"resumed_state\":\"...\"} on success, \
     or {\"status\":\"timeout\"} if `wait_timeout_secs` elapses for this call. On timeout, call this tool again \
     to keep waiting. \
     Must only be called immediately after /ask_human while state is AwaitingHuman.";

pub async fn handler_for_agent(agent_id: &str) -> Result<String, Error> {
    run(agent_id).await.map_err(tool_err)
}

async fn run(agent_id: &str) -> Result<String> {
    let state = store::read_state().await?;
    if state.state != TaskState::AwaitingHuman {
        anyhow::bail!(
            "Cannot wait for answer from state {:?}; expected AwaitingHuman",
            state.state
        );
    }
    ensure_answer_waiter(&state, agent_id)?;

    let config = Config::load().await?;
    let timeout = Duration::from_secs(config.limits.wait_timeout_secs);
    let start = Instant::now();

    loop {
        match store::read_answer().await {
            Ok(ans) if !ans.trim().is_empty() => {
                // Answer is available — restore paused state and return it.
                let mut state = store::read_state().await?;
                if state.state != TaskState::AwaitingHuman {
                    anyhow::bail!(
                        "Cannot wait for answer from state {:?}; expected AwaitingHuman",
                        state.state
                    );
                }
                ensure_answer_waiter(&state, agent_id)?;
                let resumed = state.answer()?;
                store::write_state(&state).await?;
                store::clear_answer().await?;
                store::clear_question().await?;

                let answer = ans.trim().to_string();
                info!(resumed = ?resumed, "Human answered; state restored");
                let response = json!({
                    "status": "answered",
                    "answer": answer,
                    "resumed_state": format!("{resumed:?}"),
                });
                return Ok(response.to_string());
            }
            _ => {}
        }

        if start.elapsed() >= timeout {
            info!("wait_for_answer timed out");
            let response = json!({"status": "timeout"});
            return Ok(response.to_string());
        }

        sleep(Duration::from_secs(2)).await;
    }
}
