use anyhow::Result;
use neva::prelude::*;

use crate::state::store;

use super::tool_err;

#[tool(descr = "Query the current state of the ferrus orchestration system. Returns state, \
                retry counters, and any failure reason. Safe to call from any state.")]
async fn status() -> Result<String, Error> {
    run().await.map_err(tool_err)
}

async fn run() -> Result<String> {
    let state = store::read_state().await?;

    let mut lines = vec![
        format!("**State:** {:?}", state.state),
        format!("**Check retries:** {}", state.check_retries),
        format!("**Review cycles:** {}", state.review_cycles),
    ];

    if let Some(reason) = &state.failure_reason {
        lines.push(format!("**Failure reason:** {reason}"));
    }

    Ok(lines.join("\n"))
}
