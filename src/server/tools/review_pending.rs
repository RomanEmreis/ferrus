use anyhow::Result;
use neva::prelude::*;
use tracing::info;

use crate::{
    config::Config,
    project::RuntimeTaskContext,
    state::{machine::TaskState, store},
};

use super::{ensure_lease_owner_or_reclaim, runtime_task_context_for_agent_best_effort, tool_err};

pub const DESCRIPTION: &str = "Retrieve the pending submission for review. Returns the task description, \
     the Executor's submission notes (summary, verification steps, known limitations), \
     and any prior review notes. Only valid in state Reviewing.";

pub async fn handler_for_agent(agent_id: &str) -> Result<String, Error> {
    run(agent_id).await.map_err(tool_err)
}

async fn run(agent_id: &str) -> Result<String> {
    let config = Config::load().await?;
    let mut state = store::read_state().await?;
    let runtime_context = runtime_task_context_for_agent_best_effort(agent_id).await;

    if state.state != TaskState::Reviewing
        && !matches!(
            runtime_context
                .as_ref()
                .map(|context| context.status.as_str()),
            Some("reviewing")
        )
    {
        anyhow::bail!(
            "No submission pending review. Current state: {:?}. \
             Wait for the Executor to call /submit.",
            state.state
        );
    }
    ensure_lease_owner_or_reclaim(&mut state, agent_id, config.lease.ttl_secs).await?;

    let (task, submission, review) = read_review_context(runtime_context.as_ref()).await?;

    let mut response = format!("## Task\n\n{task}\n");

    if !submission.trim().is_empty() {
        response.push_str("\n## Submission Notes\n\n");
        response.push_str(&submission);
    }

    if !review.trim().is_empty() {
        response.push_str("\n## Previous Review Notes\n\n");
        response.push_str(&review);
    }

    response.push_str(&format!(
        "\n---\nReview cycles used: {}/{}  \nCheck retries used: {}/{}",
        state.review_cycles,
        config.limits.max_review_cycles,
        state.check_retries,
        config.limits.max_check_retries,
    ));

    info!("Supervisor fetched pending review");
    Ok(response)
}

async fn read_review_context(
    context: Option<&RuntimeTaskContext>,
) -> Result<(String, String, String)> {
    if let Some(context) = context {
        return Ok((
            store::read_task_at(&context.task_path).await?,
            store::read_submission_for_run_dir(&context.run_dir).await?,
            store::read_review_for_run_dir(&context.run_dir).await?,
        ));
    }

    Ok((
        store::read_task().await?,
        store::read_submission().await?,
        store::read_review().await?,
    ))
}
