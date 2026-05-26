use anyhow::Result;
use neva::prelude::*;
use tracing::info;

use crate::{
    project::{self, RuntimeTaskContext},
    state::{machine::TaskState, store},
};

use super::{runtime_task_context_for_agent_best_effort, tool_err, uses_legacy_state_context};

pub const DESCRIPTION: &str = "Human escape hatch: reset a Failed task row. DB-first for scoped \
     runtime tasks; legacy STATE.json fallback is retained for manually started old sessions.";

pub async fn handler_for_agent(agent_id: &str) -> Result<String, Error> {
    run_for_agent(Some(agent_id)).await.map_err(tool_err)
}

async fn run_for_agent(agent_id: Option<&str>) -> Result<String> {
    let state = store::read_state().await.ok();
    let runtime_context = match agent_id {
        Some(agent_id) => runtime_task_context_for_agent_best_effort(agent_id).await,
        None => None,
    };
    if !uses_legacy_state_context(state.as_ref(), runtime_context.as_ref())
        && let Some(context) = runtime_context.as_ref()
    {
        return reset_runtime_task(context).await;
    }

    let mut state =
        state.ok_or_else(|| anyhow::anyhow!("Cannot reset legacy state: STATE.json is missing"))?;

    if state.state != TaskState::Failed {
        anyhow::bail!(
            "Cannot reset from state {:?}. Reset is only available in the Failed state.",
            state.state
        );
    }

    let active_task = state
        .active_task_id
        .clone()
        .zip(state.active_task_path.clone());

    store::clear_review_for_state(&state).await?;
    store::clear_submission_for_state(&state).await?;
    store::clear_consult_request().await?;
    store::clear_consult_response().await?;
    state.reset()?;
    store::write_state(&state).await?;

    if let Some((task_id, task_path)) = active_task {
        project::record_task_status_best_effort(&task_id, &task_path, "reset").await;
    }
    project::record_current_task_status_best_effort("idle").await;
    project::record_runtime_event_best_effort(None, "reset", serde_json::json!({})).await;

    info!("State reset, Idle");
    Ok("State reset to Idle. REVIEW.md, SUBMISSION.md, and consultation files cleared. Ready for a new task.".to_string())
}

async fn reset_runtime_task(context: &RuntimeTaskContext) -> Result<String> {
    if context.status != "failed" {
        anyhow::bail!(
            "Cannot reset task {} from status {}. Reset is only available for failed tasks.",
            context.task_id,
            context.status
        );
    }

    project::record_task_status_with_origin(
        &context.task_id,
        &context.task_path,
        "reset",
        None,
        None,
    )
    .await?;
    project::record_runtime_event_best_effort(
        context.run_id.clone(),
        "reset",
        serde_json::json!({ "task_id": context.task_id }),
    )
    .await;

    info!(task_id = context.task_id, "Task reset in ferrus.db");
    Ok(format!(
        "Task {} reset. State: reset. The task artifact remains at {}.",
        context.task_id, context.task_path
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn setup() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        let data_dir = dir.path().join(".ferrus/projects/test-project");
        std::fs::create_dir_all(dir.path().join(".ferrus")).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();
        let local_ref = crate::project::LocalProjectRef {
            project_id: "test-project".to_string(),
            name: "test".to_string(),
            data_dir: data_dir.display().to_string(),
        };
        let local_ref = toml::to_string_pretty(&local_ref).unwrap();
        std::fs::write(dir.path().join(".ferrus/project.toml"), local_ref).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        (dir, previous)
    }

    fn teardown(previous: std::path::PathBuf) {
        std::env::set_current_dir(previous).unwrap();
    }

    #[tokio::test]
    async fn reset_uses_database_context_when_state_json_is_absent() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status("t-007", ".ferrus/tasks/t-007.md", "failed")
            .await
            .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "executor:codex:7", 60)
            .await
            .unwrap();

        let response = run_for_agent(Some("executor:codex:7")).await.unwrap();

        assert!(response.contains("Task t-007 reset"));
        assert!(crate::state::store::read_state().await.is_err());
        let tasks = crate::project::list_tasks().await.unwrap();
        assert_eq!(tasks[0].status, "reset");

        teardown(previous);
    }

    #[tokio::test]
    async fn reset_rejects_non_failed_database_task() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status("t-007", ".ferrus/tasks/t-007.md", "executing")
            .await
            .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "executor:codex:7", 60)
            .await
            .unwrap();

        let message = run_for_agent(Some("executor:codex:7"))
            .await
            .unwrap_err()
            .to_string();

        assert!(message.contains("Reset is only available for failed tasks"));
        let tasks = crate::project::list_tasks().await.unwrap();
        assert_eq!(tasks[0].status, "executing");

        teardown(previous);
    }
}
