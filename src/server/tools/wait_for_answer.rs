use anyhow::Result;
use neva::prelude::*;
use serde_json::json;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::info;

use crate::{config::Config, state::store};

use super::tool_err;

pub const DESCRIPTION: &str =
    "Block until the human provides an answer to the question you asked via /ask_human. \
     Polls .ferrus/ANSWER.md until it has content, then restores the paused state and \
     returns the answer. \
     Returns {\"status\":\"answered\", \"answer\":\"...\", \"resumed_state\":\"...\"} on success, \
     or {\"status\":\"timeout\"} if wait_timeout_secs elapses. On timeout, call this tool again \
     to keep waiting. \
     Must only be called immediately after /ask_human while state is AwaitingHuman.";

pub async fn handler() -> Result<String, Error> {
    run().await.map_err(tool_err)
}

async fn run() -> Result<String> {
    let config = Config::load().await?;
    let timeout = Duration::from_secs(config.limits.wait_timeout_secs);
    let start = Instant::now();

    loop {
        match store::read_answer().await {
            Ok(ans) if !ans.trim().is_empty() => {
                // Answer is available — restore paused state and return it.
                let mut state = store::read_state().await?;
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
