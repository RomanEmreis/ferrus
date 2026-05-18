use anyhow::Result;
use neva::prelude::*;
use tracing::info;

use crate::{
    config::Config,
    project::{self, RuntimeTaskContext},
    specs,
    state::{machine::TaskState, store},
};

use super::{ensure_lease_owner_or_reclaim, runtime_task_context_for_agent_best_effort, tool_err};

pub const DESCRIPTION: &str = "Approve the current submission. Transitions state Reviewing → Complete. \
     Must be called after /review_pending.";

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
            "Cannot approve from state {:?}. Call /review_pending first.",
            state.state
        );
    }
    ensure_lease_owner_or_reclaim(&mut state, agent_id, config.lease.ttl_secs).await?;

    if should_use_legacy_state(&state, runtime_context.as_ref()) {
        specs::complete_task_milestone_and_advance(&mut state).await?;
        state.approve()?;
        store::write_state(&state).await?;
        project::record_current_task_status_best_effort("complete").await;
    } else if let Some(context) = runtime_context.as_ref() {
        project::record_task_status_best_effort(&context.task_id, &context.task_path, "complete")
            .await;
    }
    project::record_runtime_event_best_effort(
        runtime_context
            .as_ref()
            .and_then(|context| context.run_id.clone()),
        "approved",
        serde_json::json!({
            "task_id": runtime_context.as_ref().map(|context| context.task_id.as_str()),
        }),
    )
    .await;

    info!("Task approved, state → Complete");
    Ok("Task approved. State: Complete. Well done!".to_string())
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
    async fn approve_updates_agent_review_task_without_resetting_active_state() {
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

        run("supervisor:codex:7").await.unwrap();

        let state = store::read_state().await.unwrap();
        assert_eq!(state.state, TaskState::Executing);
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "complete");
        assert_eq!(task.claimed_by, None);

        teardown(previous);
    }
}
