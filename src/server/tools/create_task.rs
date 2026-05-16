use anyhow::Result;
use neva::prelude::*;
use tracing::info;

use crate::{
    project,
    state::{machine::TaskState, store},
};

use super::tool_err;

pub const DESCRIPTION: &str = "Create a new task for the Executor. Transitions state Idle → Executing and writes \
     the task description to TASK.md. Must be called from state Idle.";

pub const INPUT_SCHEMA: &str = r#"{
    "properties": {
        "description": {
            "type": "string",
            "description": "Full task description in Markdown"
        }
    },
    "required": ["description"]
}"#;

pub async fn handler(description: String) -> Result<String, Error> {
    run(description).await.map_err(tool_err)
}

async fn run(description: String) -> Result<String> {
    let mut state = store::read_state().await?;

    if state.state != TaskState::Idle {
        anyhow::bail!(
            "Cannot create task: current state is {:?}. \
             The executor must complete or reset the current task first.",
            state.state
        );
    }

    let artifact = project::allocate_task_artifact().await?;
    state.create_task()?;
    state.set_active_task_artifacts(
        artifact.id.clone(),
        artifact.path.clone(),
        artifact.run_dir.clone(),
    );
    store::write_task_for_state(&state, &description).await?;
    store::clear_submission_for_state(&state).await?;
    store::clear_consult_request().await?;
    store::clear_consult_response().await?;
    store::write_state(&state).await?;
    project::record_current_task_status_best_effort("executing").await;
    project::record_runtime_event_best_effort(
        None,
        "task_created",
        serde_json::json!({
            "task_id": artifact.id,
            "path": artifact.path,
            "run_dir": artifact.run_dir,
            "description_bytes": description.len(),
        }),
    )
    .await;

    info!("Task created, state → Executing");
    Ok("Task created. State: Executing. The Executor can now call /wait_for_task.".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{machine::StateData, store};
    use tempfile::TempDir;

    async fn setup() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ferrus")).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        store::write_state(&StateData::default()).await.unwrap();
        (dir, previous)
    }

    fn teardown(previous: std::path::PathBuf) {
        std::env::set_current_dir(previous).unwrap();
    }

    #[tokio::test]
    async fn create_task_writes_numbered_task_artifact_and_legacy_mirror() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;

        run("Build the thing".to_string()).await.unwrap();

        let state = store::read_state().await.unwrap();
        assert_eq!(state.state, TaskState::Executing);
        assert_eq!(state.active_task_id.as_deref(), Some("t-001"));
        assert_eq!(
            state.active_task_path.as_deref(),
            Some(".ferrus/tasks/t-001.md")
        );
        assert_eq!(state.active_run_dir.as_deref(), Some(".ferrus/runs/t-001"));
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/tasks/t-001.md")
                .await
                .unwrap(),
            "Build the thing"
        );
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/TASK.md").await.unwrap(),
            "Build the thing"
        );

        teardown(previous);
    }
}
