use anyhow::Result;
use neva::prelude::*;
use tracing::info;

use crate::{
    config::Config,
    project,
    state::{machine::TaskState, machine::TransitionError, store},
};

use super::{
    check_gate::{self, CheckGateResult},
    ensure_lease_owner_or_reclaim, tool_err,
};

pub const DESCRIPTION: &str = "\
Run the final check gate and, if it passes, submit work for Supervisor review. \
Can be called from Executing or Addressing. \
On pass: state → Reviewing. On fail: stay in the current work state (or state \
→ Failed if the retry limit is exhausted).

The `content` parameter must be a Markdown document with the following sections:

## Summary
Brief description of what was changed and why.

## How to verify manually
Step-by-step instructions for the Supervisor to spot-check the work.

## Known limitations
Anything deliberately left out, edge cases not handled, or follow-up work needed. \
Omit this section if there are none.";

pub const INPUT_SCHEMA: &str = r#"{
    "properties": {
        "content": {
            "type": "string",
            "description": "Submission notes in Markdown (summary, how to verify, known limitations)"
        }
    },
    "required": ["content"]
}"#;

pub async fn handler_for_agent(agent_id: &str, content: String) -> Result<String, Error> {
    run(Some(agent_id), content).await.map_err(tool_err)
}

async fn run(agent_id: Option<&str>, content: String) -> Result<String> {
    let config = Config::load().await?;
    let mut state = store::read_state().await?;

    if !matches!(state.state, TaskState::Executing | TaskState::Addressing) {
        anyhow::bail!(
            "Cannot submit from state {:?}. Submit is only valid from Executing or Addressing after the implementation is ready.",
            state.state
        );
    }
    if let Some(agent_id) = agent_id {
        ensure_lease_owner_or_reclaim(&mut state, agent_id, config.lease.ttl_secs).await?;
    }

    if config.checks.commands.is_empty() {
        info!("No check commands configured; treating final check gate as pass");
        state.check_passed()?;
        state.submit()?;
        store::write_submission(&content).await?;
        store::write_state(&state).await?;
        project::record_current_task_status_best_effort("reviewing").await;
        project::record_runtime_event_best_effort(
            None,
            "submitted",
            serde_json::json!({ "content_bytes": content.len(), "check_gate": "skipped" }),
        )
        .await;

        return Ok(
            "Submitted for review. Warning: no check commands are configured in ferrus.toml, so the final check gate was treated as a pass. State: Reviewing."
                .to_string(),
        );
    }

    info!("Running final check gate before review submission");
    match check_gate::run(&config, state.check_retries + 1).await? {
        CheckGateResult::Passed => {
            state.check_passed()?;
            state.submit()?;
            store::write_submission(&content).await?;
            store::write_state(&state).await?;
            project::record_current_task_status_best_effort("reviewing").await;
            project::record_runtime_event_best_effort(
                None,
                "submitted",
                serde_json::json!({ "content_bytes": content.len(), "check_gate": "passed" }),
            )
            .await;

            info!("Work submitted for review, state → Reviewing");
            Ok(
                "Submitted for review. State: Reviewing. The Supervisor can now call /review_pending."
                    .to_string(),
            )
        }
        CheckGateResult::Failed(failure) => {
            match state.check_failed(failure.failure_reason, config.limits.max_check_retries) {
                Ok(()) => {
                    store::write_state(&state).await?;
                    project::record_runtime_event_best_effort(
                        None,
                        "submit_check_failed",
                        serde_json::json!({
                            "retries": state.check_retries,
                            "max_retries": config.limits.max_check_retries,
                            "state": format!("{:?}", state.state),
                        }),
                    )
                    .await;
                    Ok(format!(
                        "Final review gate failed during /submit (retry {}/{}).\n\n{}\n\nState remains {:?}. Fix the issues and run /check or /submit again.",
                        state.check_retries,
                        config.limits.max_check_retries,
                        failure.report,
                        state.state,
                    ))
                }
                Err(TransitionError::CheckLimitExceeded { retries }) => {
                    store::write_state(&state).await?;
                    project::record_current_task_status_best_effort("failed").await;
                    project::record_runtime_event_best_effort(
                        None,
                        "submit_check_limit_exceeded",
                        serde_json::json!({
                            "retries": retries,
                            "max_retries": config.limits.max_check_retries,
                        }),
                    )
                    .await;
                    Ok(format!(
                        "Final review gate failed during /submit and hit the retry limit ({retries}/{}).\n\n{}\n\nState is now Failed. A human must call /reset to recover.",
                        config.limits.max_check_retries, failure.report,
                    ))
                }
                Err(e) => anyhow::bail!(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::machine::StateData;
    use chrono::Utc;
    use tempfile::TempDir;

    async fn setup() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ferrus")).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        tokio::fs::write(
            "ferrus.toml",
            "[checks]\ncommands = []\n\n[limits]\nmax_check_retries = 20\nmax_review_cycles = 3\nmax_feedback_lines = 30\nwait_timeout_secs = 60\n",
        )
        .await
        .unwrap();
        (dir, previous)
    }

    fn teardown(previous: std::path::PathBuf) {
        std::env::set_current_dir(previous).unwrap();
    }

    #[tokio::test]
    async fn submit_reclaims_expired_same_agent_lease_before_guarding() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        let mut state = StateData {
            state: TaskState::Executing,
            claimed_by: Some("executor:codex:1".to_string()),
            lease_until: Some(Utc::now() - chrono::Duration::seconds(1)),
            last_heartbeat: Some(Utc::now() - chrono::Duration::seconds(2)),
            ..StateData::default()
        };
        state.set_active_task_artifacts(
            "t-001".to_string(),
            ".ferrus/tasks/t-001.md".to_string(),
            ".ferrus/runs/t-001".to_string(),
        );
        store::write_state(&state).await.unwrap();

        run(
            Some("executor:codex:1"),
            "## Summary\nDone.\n\n## How to verify manually\nInspect it.\n".to_string(),
        )
        .await
        .unwrap();

        let state = store::read_state().await.unwrap();
        assert_eq!(state.state, TaskState::Reviewing);
        assert!(state.claimed_by.is_none());
        assert!(state.lease_until.is_none());
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/runs/t-001/SUBMISSION.md")
                .await
                .unwrap(),
            "## Summary\nDone.\n\n## How to verify manually\nInspect it.\n"
        );

        teardown(previous);
    }
}
