use anyhow::Result;
use neva::prelude::*;
use std::path::{Path, PathBuf};
use tokio::process::Command;
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
        apply_approved_patch(context).await?;
        project::record_task_status_best_effort(&context.task_id, &context.task_path, "complete")
            .await;
        cleanup_approved_workspace_best_effort(context).await;
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

async fn apply_approved_patch(context: &RuntimeTaskContext) -> Result<()> {
    let patch = store::read_patch_for_run_dir(&context.run_dir).await?;
    if patch.trim().is_empty() {
        return Ok(());
    }

    let project_root = std::env::current_dir()?;
    let patch_path = store::resolve_project_path(Path::new(&context.run_dir).join("PATCH.diff"));
    let output = Command::new("git")
        .arg("-C")
        .arg(&project_root)
        .args(["apply", "--whitespace=nowarn"])
        .arg(&patch_path)
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "Cannot approve task {} because its patch could not be applied to {}: {}",
            context.task_id,
            project_root.display(),
            if stderr.is_empty() {
                output.status.to_string()
            } else {
                stderr
            }
        );
    }
    Ok(())
}

async fn cleanup_approved_workspace_best_effort(context: &RuntimeTaskContext) {
    if let Err(err) = cleanup_approved_workspace(context).await {
        tracing::warn!(
            error = ?err,
            task_id = context.task_id,
            workspace_path = context.workspace_path.as_deref(),
            "failed to remove approved task worktree"
        );
    }
}

async fn cleanup_approved_workspace(context: &RuntimeTaskContext) -> Result<bool> {
    let Some(workspace_path) = context
        .workspace_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
    else {
        return Ok(false);
    };
    if !tokio::fs::try_exists(&workspace_path).await? {
        return Ok(false);
    }

    let local_ref_content =
        tokio::fs::read_to_string(store::resolve_project_path(".ferrus/project.toml")).await?;
    let local_ref: project::LocalProjectRef = toml::from_str(&local_ref_content)?;
    let managed_root = PathBuf::from(local_ref.data_dir).join("worktrees");
    let canonical_workspace = tokio::fs::canonicalize(&workspace_path)
        .await
        .unwrap_or(workspace_path);
    let canonical_managed_root = tokio::fs::canonicalize(&managed_root)
        .await
        .unwrap_or(managed_root);
    if !is_managed_workspace_path(&canonical_workspace, &canonical_managed_root) {
        return Ok(false);
    }

    let project_root = std::env::current_dir()?;
    let output = Command::new("git")
        .arg("-C")
        .arg(&project_root)
        .args(["worktree", "remove", "--force"])
        .arg(&canonical_workspace)
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "Failed to remove approved task worktree at {}: {}",
            canonical_workspace.display(),
            if stderr.is_empty() {
                output.status.to_string()
            } else {
                stderr
            }
        );
    }
    Ok(true)
}

fn is_managed_workspace_path(path: &Path, managed_root: &Path) -> bool {
    path.starts_with(managed_root) && path != managed_root
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

    #[tokio::test]
    async fn approve_applies_scoped_patch_before_marking_task_complete() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (dir, previous) = setup().await;
        if !git(dir.path(), ["init"]).success() {
            teardown(previous);
            return;
        }
        tokio::fs::write("file.txt", "old\n").await.unwrap();
        assert!(git(dir.path(), ["add", "file.txt"]).success());
        assert!(
            git(
                dir.path(),
                [
                    "-c",
                    "user.email=ferrus@example.invalid",
                    "-c",
                    "user.name=Ferrus",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-m",
                    "initial",
                ],
            )
            .success()
        );
        tokio::fs::write("file.txt", "new\n").await.unwrap();
        let patch = git_output(dir.path(), ["diff", "--binary", "HEAD", "--", "file.txt"]);
        tokio::fs::write("file.txt", "old\n").await.unwrap();
        assert!(!patch.trim().is_empty());

        store::write_state(&StateData::default()).await.unwrap();
        store::write_patch_for_run_dir(".ferrus/runs/t-007", &patch)
            .await
            .unwrap();
        crate::project::record_task_status("t-007", ".ferrus/tasks/t-007.md", "reviewing")
            .await
            .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "supervisor:codex:7", 60)
            .await
            .unwrap();

        run("supervisor:codex:7").await.unwrap();

        let file = tokio::fs::read_to_string("file.txt").await.unwrap();
        assert_eq!(file.replace("\r\n", "\n"), "new\n");
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "complete");

        teardown(previous);
    }

    #[tokio::test]
    async fn approve_removes_managed_worktree_after_completion() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (dir, previous) = setup().await;
        if !git(dir.path(), ["init"]).success() {
            teardown(previous);
            return;
        }
        tokio::fs::write("file.txt", "base\n").await.unwrap();
        assert!(git(dir.path(), ["add", "file.txt"]).success());
        assert!(
            git(
                dir.path(),
                [
                    "-c",
                    "user.email=ferrus@example.invalid",
                    "-c",
                    "user.name=Ferrus",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-m",
                    "initial",
                ],
            )
            .success()
        );

        let workspace_path = dir
            .path()
            .join(".ferrus/projects/test-project/worktrees/t-007");
        assert!(
            git_path(
                dir.path(),
                ["worktree", "add", "--detach"],
                &workspace_path,
                ["HEAD"],
            )
            .success()
        );
        assert!(workspace_path.is_dir());

        store::write_state(&StateData::default()).await.unwrap();
        crate::project::record_task_status("t-007", ".ferrus/tasks/t-007.md", "reviewing")
            .await
            .unwrap();
        let run_record = crate::project::record_run_started_with_workspace(
            "supervisor-run-t-007",
            "supervisor",
            "supervisor:codex:7",
            std::process::id(),
            workspace_path.to_string_lossy().into_owned(),
        )
        .await
        .unwrap();
        let attached = crate::project::attach_running_run_to_task(
            "supervisor:codex:7",
            "t-007",
            ".ferrus/tasks/t-007.md",
        )
        .await
        .unwrap();
        assert_eq!(attached.as_deref(), Some(run_record.id.as_str()));
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "supervisor:codex:7", 60)
            .await
            .unwrap();
        let context = crate::project::runtime_task_context_for_agent("supervisor:codex:7")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            context.workspace_path.as_deref(),
            Some(workspace_path.to_string_lossy().as_ref())
        );

        run("supervisor:codex:7").await.unwrap();

        assert!(!workspace_path.exists());
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "complete");

        teardown(previous);
    }

    fn git<const N: usize>(cwd: &std::path::Path, args: [&str; N]) -> std::process::ExitStatus {
        std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .status()
            .unwrap()
    }

    fn git_path<const N: usize, const M: usize>(
        cwd: &std::path::Path,
        before: [&str; N],
        path: &std::path::Path,
        after: [&str; M],
    ) -> std::process::ExitStatus {
        std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(before)
            .arg(path)
            .args(after)
            .status()
            .unwrap()
    }

    fn git_output<const N: usize>(cwd: &std::path::Path, args: [&str; N]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
}
