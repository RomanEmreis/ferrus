use anyhow::Result;
use fs2::FileExt;
use neva::prelude::*;
use serde_json::json;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::info;

use crate::{
    agent_id::ENV_TASK_ID,
    config::Config,
    project::{self, ReadyTaskClaim, TaskLease},
    state::{
        machine::{StateData, TaskState},
        store,
    },
};

use super::tool_err;

pub const DESCRIPTION: &str = "Block until the Executor submits work for review, then atomically claim the review and \
     return the full submission context. \
     Returns a JSON object: {\"status\":\"claimed\", \"claimed_by\":\"...\", \"lease_until\":\"...\", \
     \"state\":\"Reviewing\", \"task\":\"...\", \"submission\":\"...\", \"review\":\"...\"} \
     when a submission is ready, or {\"status\":\"timeout\", \"state\":\"...\"} on timeout. \
     Each call waits up to `wait_timeout_secs` (see ferrus.toml), then returns timeout so the \
     agent can poll again. \
     Returns immediately if a submission is already pending — safe to call on restart.";

pub async fn handler(agent_id: &str) -> Result<String, Error> {
    run(agent_id).await.map_err(tool_err)
}

async fn run(agent_id: &str) -> Result<String> {
    let config = Config::load().await?;
    let timeout = Duration::from_secs(config.limits.wait_timeout_secs);
    let ttl_secs = config.lease.ttl_secs;
    let start = Instant::now();

    loop {
        let claim = {
            let lock_file = store::open_lock_file()?;
            let lock_file = tokio::task::spawn_blocking(move || -> Result<std::fs::File> {
                lock_file.lock_exclusive().map_err(anyhow::Error::from)?;
                Ok(lock_file)
            })
            .await??;

            let mut state = store::read_state()
                .await
                .unwrap_or_else(|_| StateData::default());
            let claim = claim_review_task(agent_id, ttl_secs, &mut state).await?;

            drop(lock_file);
            claim
        };

        if let Some(claim) = claim {
            project::attach_running_run_to_task_best_effort(
                agent_id,
                &claim.task_id,
                &claim.task_path,
            )
            .await;
            let run_dir = run_dir_for_task(&claim.task_id);
            let task = store::read_task_at(&claim.task_path).await?;
            let submission = store::read_submission_for_run_dir(&run_dir).await?;
            let review = store::read_review_for_run_dir(&run_dir).await?;
            let patch = store::read_patch_for_run_dir(&run_dir).await?;

            info!(
                agent_id,
                task_id = claim.task_id,
                "Supervisor claimed review"
            );
            let response = json!({
                "status": "claimed",
                "task_id": claim.task_id,
                "task_path": claim.task_path,
                "run_dir": run_dir,
                "claimed_by": claim.claimed_by,
                "lease_until": claim.lease_until,
                "state": "Reviewing",
                "task": task,
                "submission": submission,
                "review": review,
                "patch": patch,
                "review_cycles_used": claim.review_cycles,
                "check_retries_used": claim.check_retries,
            });
            return Ok(response.to_string());
        }

        if start.elapsed() >= timeout {
            let state = store::read_state().await.ok();
            let state_label = state
                .as_ref()
                .map(|state| format!("{:?}", state.state))
                .unwrap_or_else(|| "unavailable".to_string());
            info!("wait_for_review timed out, state: {state_label}");
            let response = json!({
                "status": "timeout",
                "state": state_label,
            });
            return Ok(response.to_string());
        }

        sleep(Duration::from_millis(500)).await;
    }
}

async fn claim_review_task(
    agent_id: &str,
    ttl_secs: u64,
    state: &mut crate::state::machine::StateData,
) -> Result<Option<TaskLease>> {
    if let Some(task_id) = runtime_task_id() {
        return claim_runtime_review_task(&task_id, agent_id, ttl_secs).await;
    }

    match project::claim_next_review_task(agent_id, ttl_secs).await {
        Ok(ReadyTaskClaim::Claimed(task)) | Ok(ReadyTaskClaim::AlreadyClaimed(task)) => {
            mirror_state_lease_if_current_task(state, &task).await?;
            Ok(Some(task))
        }
        Ok(ReadyTaskClaim::NoAvailable) => Ok(None),
        Err(err) => {
            tracing::warn!(
                error = ?err,
                "failed to claim next review task in ferrus.db; falling back to STATE.json lease"
            );
            claim_state_fallback(agent_id, ttl_secs, state).await
        }
    }
}

async fn claim_runtime_review_task(
    task_id: &str,
    agent_id: &str,
    ttl_secs: u64,
) -> Result<Option<TaskLease>> {
    match project::claim_review_task_by_id(task_id, agent_id, ttl_secs).await? {
        ReadyTaskClaim::Claimed(task) | ReadyTaskClaim::AlreadyClaimed(task) => Ok(Some(task)),
        ReadyTaskClaim::NoAvailable => Ok(None),
    }
}

fn runtime_task_id() -> Option<String> {
    std::env::var(ENV_TASK_ID)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

async fn mirror_state_lease_if_current_task(
    _state: &mut crate::state::machine::StateData,
    _task: &TaskLease,
) -> Result<()> {
    Ok(())
}

async fn claim_state_fallback(
    agent_id: &str,
    ttl_secs: u64,
    state: &mut crate::state::machine::StateData,
) -> Result<Option<TaskLease>> {
    if state.state != TaskState::Reviewing {
        return Ok(None);
    }

    if !state.is_claimed() {
        store::claim_state(agent_id, ttl_secs, state).await?;
    } else if !state.is_claimed_by(agent_id) {
        return Ok(None);
    }

    let task_id = state
        .active_task_id
        .clone()
        .unwrap_or_else(|| "current".to_string());
    let task_path = state
        .active_task_path
        .clone()
        .unwrap_or_else(|| ".ferrus/TASK.md".to_string());
    Ok(Some(TaskLease {
        task_id,
        task_path,
        status: "reviewing".to_string(),
        paused_status: state
            .paused_state
            .as_ref()
            .map(project::task_status_for_state)
            .map(str::to_string),
        check_retries: state.check_retries,
        review_cycles: state.review_cycles,
        failure_reason: state.failure_reason.clone(),
        claimed_by: agent_id.to_string(),
        lease_until: state
            .lease_until
            .ok_or_else(|| anyhow::anyhow!("claimed STATE.json review is missing lease_until"))?,
    }))
}

fn run_dir_for_task(task_id: &str) -> String {
    format!(".ferrus/runs/{task_id}")
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
        std::fs::create_dir_all(dir.path().join(".ferrus/runs/t-003")).unwrap();
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
    async fn wait_for_review_claims_next_reviewing_database_task() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        tokio::fs::write(".ferrus/tasks/t-003.md", "review task")
            .await
            .unwrap();
        tokio::fs::write(".ferrus/runs/t-003/SUBMISSION.md", "submission")
            .await
            .unwrap();
        store::write_patch_for_run_dir(".ferrus/runs/t-003", "diff --git a/a b/a\n+change\n")
            .await
            .unwrap();
        crate::project::record_task_status("t-003", ".ferrus/tasks/t-003.md", "reviewing")
            .await
            .unwrap();

        let response: serde_json::Value =
            serde_json::from_str(&run("supervisor:codex:1").await.unwrap()).unwrap();

        assert_eq!(response["status"], "claimed");
        assert_eq!(response["task_id"], "t-003");
        assert_eq!(response["task"], "review task");
        assert_eq!(response["submission"], "submission");
        assert_eq!(response["patch"], "diff --git a/a b/a\n+change\n");
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-003").unwrap();
        assert_eq!(task.claimed_by.as_deref(), Some("supervisor:codex:1"));

        teardown(previous);
    }

    #[tokio::test]
    async fn wait_for_review_claims_database_task_when_state_json_is_absent() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (dir, previous) = setup().await;
        tokio::fs::remove_file(dir.path().join(".ferrus/STATE.json"))
            .await
            .unwrap();
        tokio::fs::write(".ferrus/tasks/t-003.md", "review task")
            .await
            .unwrap();
        tokio::fs::write(".ferrus/runs/t-003/SUBMISSION.md", "submission")
            .await
            .unwrap();
        crate::project::record_task_status("t-003", ".ferrus/tasks/t-003.md", "reviewing")
            .await
            .unwrap();

        let response: serde_json::Value =
            serde_json::from_str(&run("supervisor:codex:3").await.unwrap()).unwrap();

        assert_eq!(response["status"], "claimed");
        assert_eq!(response["task_id"], "t-003");
        assert_eq!(response["submission"], "submission");

        teardown(previous);
    }
}
