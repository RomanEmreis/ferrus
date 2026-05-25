use anyhow::Result;
use neva::prelude::*;
use tracing::info;

use crate::{
    config::Config,
    project::RuntimeTaskContext,
    state::{
        machine::{StateData, TaskState},
        store,
    },
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
    let runtime_context = runtime_task_context_for_agent_best_effort(agent_id).await;
    let mut state = store::read_state().await.ok();

    let context_is_reviewing = matches!(
        runtime_context
            .as_ref()
            .map(|context| context.status.as_str()),
        Some("reviewing")
    );
    let state_is_reviewing = state
        .as_ref()
        .is_some_and(|state| state.state == TaskState::Reviewing);
    if !context_is_reviewing && !state_is_reviewing {
        let current_state = state
            .as_ref()
            .map(|state| format!("{:?}", state.state))
            .unwrap_or_else(|| "unavailable".to_string());
        anyhow::bail!(
            "No submission pending review. Current state: {current_state}. \
             Wait for the Executor to call /submit.",
        );
    }
    let state_for_lease = state.get_or_insert_with(StateData::default);
    ensure_lease_owner_or_reclaim(state_for_lease, agent_id, config.lease.ttl_secs).await?;

    let (task, submission, review, patch, integration_error) =
        read_review_context(runtime_context.as_ref()).await?;

    let mut response = format!("## Task\n\n{task}\n");

    if !submission.trim().is_empty() {
        response.push_str("\n## Submission Notes\n\n");
        response.push_str(&submission);
    }

    if !review.trim().is_empty() {
        response.push_str("\n## Previous Review Notes\n\n");
        response.push_str(&review);
    }

    if !patch.trim().is_empty() {
        response.push_str("\n## Implementation Patch\n\n```diff\n");
        response.push_str(&patch);
        if !patch.ends_with('\n') {
            response.push('\n');
        }
        response.push_str("```\n");
    }

    if !integration_error.trim().is_empty() {
        response.push_str("\n## Integration Error\n\n");
        response.push_str(&integration_error);
        if !integration_error.ends_with('\n') {
            response.push('\n');
        }
    }

    let review_cycles = runtime_context
        .as_ref()
        .map(|context| context.review_cycles)
        .or_else(|| state.as_ref().map(|state| state.review_cycles))
        .unwrap_or_default();
    let check_retries = runtime_context
        .as_ref()
        .map(|context| context.check_retries)
        .or_else(|| state.as_ref().map(|state| state.check_retries))
        .unwrap_or_default();

    response.push_str(&format!(
        "\n---\nReview cycles used: {}/{}  \nCheck retries used: {}/{}",
        review_cycles,
        config.limits.max_review_cycles,
        check_retries,
        config.limits.max_check_retries,
    ));

    info!("Supervisor fetched pending review");
    Ok(response)
}

async fn read_review_context(
    context: Option<&RuntimeTaskContext>,
) -> Result<(String, String, String, String, String)> {
    if let Some(context) = context {
        return Ok((
            store::read_task_at(&context.task_path).await?,
            store::read_submission_for_run_dir(&context.run_dir).await?,
            store::read_review_for_run_dir(&context.run_dir).await?,
            store::read_patch_for_run_dir(&context.run_dir).await?,
            store::read_integration_error_for_run_dir(&context.run_dir).await?,
        ));
    }

    Ok((
        store::read_task().await?,
        store::read_submission().await?,
        store::read_review().await?,
        String::new(),
        String::new(),
    ))
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
        std::fs::create_dir_all(dir.path().join(".ferrus/runs/t-007")).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        tokio::fs::write(
            "ferrus.toml",
            "[checks]\ncommands = []\n\n[limits]\nmax_check_retries = 20\nmax_review_cycles = 3\nmax_feedback_lines = 30\nwait_timeout_secs = 1\n\n[lease]\nttl_secs = 60\n",
        )
        .await
        .unwrap();
        tokio::fs::write(".ferrus/STATE.lock", "").await.unwrap();
        store::write_state(&StateData::default()).await.unwrap();
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
    async fn review_pending_includes_scoped_implementation_patch() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        tokio::fs::write(".ferrus/tasks/t-007.md", "task body")
            .await
            .unwrap();
        store::write_submission_for_run_dir(".ferrus/runs/t-007", "submission")
            .await
            .unwrap();
        store::write_patch_for_run_dir(
            ".ferrus/runs/t-007",
            "diff --git a/file.txt b/file.txt\n+new line\n",
        )
        .await
        .unwrap();
        crate::project::record_task_status("t-007", ".ferrus/tasks/t-007.md", "reviewing")
            .await
            .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "supervisor:codex:7", 60)
            .await
            .unwrap();

        let response = run("supervisor:codex:7").await.unwrap();

        assert!(response.contains("## Implementation Patch"));
        assert!(response.contains("```diff"));
        assert!(response.contains("diff --git a/file.txt b/file.txt"));

        teardown(previous);
    }

    #[tokio::test]
    async fn review_pending_includes_scoped_integration_error() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        tokio::fs::write(".ferrus/tasks/t-007.md", "task body")
            .await
            .unwrap();
        store::write_integration_error_for_run_dir(
            ".ferrus/runs/t-007",
            "# Integration Error\n\npatch failed\n",
        )
        .await
        .unwrap();
        crate::project::record_task_status("t-007", ".ferrus/tasks/t-007.md", "reviewing")
            .await
            .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "supervisor:codex:7", 60)
            .await
            .unwrap();

        let response = run("supervisor:codex:7").await.unwrap();

        assert!(response.contains("## Integration Error"));
        assert!(response.contains("patch failed"));

        teardown(previous);
    }

    #[tokio::test]
    async fn review_pending_uses_database_context_when_state_json_is_absent() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (dir, previous) = setup().await;
        tokio::fs::remove_file(dir.path().join(".ferrus/STATE.json"))
            .await
            .unwrap();
        tokio::fs::write(".ferrus/tasks/t-007.md", "task body")
            .await
            .unwrap();
        store::write_submission_for_run_dir(".ferrus/runs/t-007", "submission")
            .await
            .unwrap();
        crate::project::record_task_status("t-007", ".ferrus/tasks/t-007.md", "reviewing")
            .await
            .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "supervisor:codex:7", 60)
            .await
            .unwrap();

        let response = run("supervisor:codex:7").await.unwrap();

        assert!(response.contains("## Task"));
        assert!(response.contains("task body"));
        assert!(response.contains("submission"));
        assert!(response.contains("Review cycles used: 0/3"));

        teardown(previous);
    }
}
