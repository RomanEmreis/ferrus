use anyhow::Result;
use neva::prelude::*;
use tracing::{info, warn};

use crate::{
    config::Config,
    project::{self, RuntimeTaskContext, TaskReviewRejection},
    state::{
        machine::{TaskState, TransitionError},
        store,
    },
};

use super::{ensure_lease_owner_or_reclaim, runtime_task_context_for_agent_best_effort, tool_err};

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
            "Cannot reject from state {:?}. Call /review_pending first.",
            state.state
        );
    }
    ensure_lease_owner_or_reclaim(&mut state, agent_id, config.lease.ttl_secs).await?;

    write_review(&state, runtime_context.as_ref(), &notes).await?;

    if !should_use_legacy_state(&state, runtime_context.as_ref())
        && let Some(context) = runtime_context.as_ref()
    {
        return match project::record_task_review_rejected(
            &context.task_id,
            config.limits.max_review_cycles,
        )
        .await?
        {
            TaskReviewRejection::Addressing { cycles } => {
                project::record_runtime_event_best_effort(
                    context.run_id.clone(),
                    "rejected",
                    serde_json::json!({
                        "task_id": context.task_id.as_str(),
                        "review_cycles": cycles,
                        "max_review_cycles": config.limits.max_review_cycles,
                        "notes_bytes": notes.len(),
                    }),
                )
                .await;
                info!(
                    review_cycles = cycles,
                    task_id = context.task_id,
                    "Submission rejected, DB task → addressing"
                );
                Ok(format!(
                    "Submission rejected (cycle {}/{}).\n\n**Review notes written.** \
                     State: Addressing. The Executor should call /wait_for_task to see the notes \
                     and /check after addressing them.",
                    cycles, config.limits.max_review_cycles,
                ))
            }
            TaskReviewRejection::LimitExceeded { cycles } => {
                project::record_runtime_event_best_effort(
                    context.run_id.clone(),
                    "review_limit_exceeded",
                    serde_json::json!({
                        "task_id": context.task_id.as_str(),
                        "review_cycles": cycles,
                        "max_review_cycles": config.limits.max_review_cycles,
                        "notes_bytes": notes.len(),
                    }),
                )
                .await;
                warn!(
                    review_cycles = cycles,
                    task_id = context.task_id,
                    "Review cycle limit reached, DB task → failed"
                );
                Ok(format!(
                    "Review cycle limit reached ({cycles}/{}).\n\nState is now Failed. \
                     A human must call /reset to recover.",
                    config.limits.max_review_cycles,
                ))
            }
        };
    }

    match state.reject(config.limits.max_review_cycles) {
        Ok(()) => {
            store::write_state(&state).await?;
            mirror_review_state(runtime_context.as_ref(), &state).await?;
            project::record_runtime_event_best_effort(
                runtime_context
                    .as_ref()
                    .and_then(|context| context.run_id.clone()),
                "rejected",
                serde_json::json!({
                    "task_id": runtime_context.as_ref().map(|context| context.task_id.as_str()),
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
            mirror_review_state(runtime_context.as_ref(), &state).await?;
            project::record_runtime_event_best_effort(
                runtime_context
                    .as_ref()
                    .and_then(|context| context.run_id.clone()),
                "review_limit_exceeded",
                serde_json::json!({
                    "task_id": runtime_context.as_ref().map(|context| context.task_id.as_str()),
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

async fn mirror_review_state(
    context: Option<&RuntimeTaskContext>,
    state: &crate::state::machine::StateData,
) -> Result<()> {
    if let Some(context) = context {
        project::mirror_task_review_state(
            &context.task_id,
            project::task_status_for_state(&state.state),
            state.review_cycles,
            state.check_retries,
            state.failure_reason.as_deref(),
        )
        .await?;
    } else {
        project::record_current_task_status_best_effort(project::task_status_for_state(
            &state.state,
        ))
        .await;
    }
    Ok(())
}

async fn write_review(
    state: &crate::state::machine::StateData,
    context: Option<&RuntimeTaskContext>,
    notes: &str,
) -> Result<()> {
    if let Some(context) = context {
        store::write_review_for_run_dir(&context.run_dir, notes).await?;
        if state.active_task_id.as_deref() == Some(context.task_id.as_str()) {
            store::write_review(notes).await?;
        }
        return Ok(());
    }
    store::write_review(notes).await
}

fn should_use_legacy_state(
    state: &crate::state::machine::StateData,
    context: Option<&RuntimeTaskContext>,
) -> bool {
    context.is_none()
        || context.is_some_and(|context| {
            state.active_task_id.as_deref() == Some(context.task_id.as_str())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::machine::StateData;
    use tempfile::TempDir;

    async fn setup() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ferrus/tasks")).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        tokio::fs::write(
            "ferrus.toml",
            "[checks]\ncommands = []\n\n[limits]\nmax_check_retries = 20\nmax_review_cycles = 3\nmax_feedback_lines = 30\nwait_timeout_secs = 1\n\n[lease]\nttl_secs = 60\n",
        )
        .await
        .unwrap();
        let data_dir = dir.path().join(".ferrus/projects/test-project");
        tokio::fs::create_dir_all(&data_dir).await.unwrap();
        let local_ref = crate::project::LocalProjectRef {
            project_id: "test-project".to_string(),
            name: "test".to_string(),
            data_dir: data_dir.to_string_lossy().into_owned(),
        };
        tokio::fs::write(
            ".ferrus/project.toml",
            toml::to_string_pretty(&local_ref).unwrap(),
        )
        .await
        .unwrap();
        (dir, previous)
    }

    fn teardown(previous: std::path::PathBuf) {
        std::env::set_current_dir(previous).unwrap();
    }

    #[tokio::test]
    async fn reject_updates_agent_review_task_and_scoped_review_notes() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        let mut state = StateData {
            state: TaskState::Executing,
            ..StateData::default()
        };
        state.set_active_task_artifacts(
            "t-001".to_string(),
            ".ferrus/tasks/t-001.md".to_string(),
            ".ferrus/runs/t-001".to_string(),
        );
        store::write_state(&state).await.unwrap();
        crate::project::record_task_status("t-007", ".ferrus/tasks/t-007.md", "reviewing")
            .await
            .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "supervisor:codex:7", 60)
            .await
            .unwrap();

        run("supervisor:codex:7", "fix this".to_string())
            .await
            .unwrap();

        let state = store::read_state().await.unwrap();
        assert_eq!(state.state, TaskState::Executing);
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/runs/t-007/REVIEW.md")
                .await
                .unwrap(),
            "fix this"
        );
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "addressing");
        assert_eq!(task.review_cycles, 1);
        assert_eq!(task.check_retries, 0);
        assert_eq!(task.claimed_by, None);

        teardown(previous);
    }

    #[tokio::test]
    async fn reject_mirrors_active_state_counters_to_database_task() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        let mut state = StateData {
            state: TaskState::Reviewing,
            review_cycles: 1,
            check_retries: 4,
            ..StateData::default()
        };
        state.set_active_task_artifacts(
            "t-001".to_string(),
            ".ferrus/tasks/t-001.md".to_string(),
            ".ferrus/runs/t-001".to_string(),
        );
        store::write_state(&state).await.unwrap();
        crate::project::record_task_status("t-001", ".ferrus/tasks/t-001.md", "reviewing")
            .await
            .unwrap();
        crate::project::claim_task("t-001", ".ferrus/tasks/t-001.md", "supervisor:codex:1", 60)
            .await
            .unwrap();

        run("supervisor:codex:1", "fix this".to_string())
            .await
            .unwrap();

        let state = store::read_state().await.unwrap();
        assert_eq!(state.state, TaskState::Addressing);
        assert_eq!(state.review_cycles, 2);
        assert_eq!(state.check_retries, 0);
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-001").unwrap();
        assert_eq!(task.status, "addressing");
        assert_eq!(task.review_cycles, 2);
        assert_eq!(task.check_retries, 0);
        assert_eq!(task.claimed_by, None);

        teardown(previous);
    }
}
