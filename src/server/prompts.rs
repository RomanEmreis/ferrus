use neva::prelude::*;

use crate::state::store;

fn to_err(e: impl std::fmt::Display) -> Error {
    Error::new(
        ErrorCode::InternalError,
        std::io::Error::other(e.to_string()),
    )
}

/// Bundles state + task + review notes for the Executor.
pub async fn executor_context() -> Result<GetPromptResult, Error> {
    let state = store::read_state().await.map_err(to_err)?;
    let task = store::read_task().await.map_err(to_err)?;

    let mut sections = vec![
        format!(
            "## State\n\nCurrent state: **{:?}** | Check retries: {} | Review cycles: {}",
            state.state, state.check_retries, state.review_cycles
        ),
        format!("## Task\n\n{task}"),
    ];

    let review = store::read_review().await.unwrap_or_default();
    if !review.trim().is_empty() {
        sections.push(format!("## Review Notes (Re-address)\n\n{review}"));
    }

    Ok(GetPromptResult::new()
        .with_descr("Executor task context: state, task description, and review notes")
        .with_message(PromptMessage::user().with(sections.join("\n\n---\n\n"))))
}

/// Bundles state + task + submission notes for the Supervisor.
pub async fn supervisor_review() -> Result<GetPromptResult, Error> {
    let state = store::read_state().await.map_err(to_err)?;
    let task = store::read_task().await.map_err(to_err)?;

    let mut sections = vec![
        format!(
            "## State\n\nCurrent state: **{:?}** | Check retries used: {} | Review cycles: {}",
            state.state, state.check_retries, state.review_cycles
        ),
        format!("## Task\n\n{task}"),
    ];

    let submission = store::read_submission().await.unwrap_or_default();
    if !submission.trim().is_empty() {
        sections.push(format!("## Submission Notes\n\n{submission}"));
    }

    Ok(GetPromptResult::new()
        .with_descr("Supervisor review context: state, task description, and submission notes")
        .with_message(PromptMessage::user().with(sections.join("\n\n---\n\n"))))
}
