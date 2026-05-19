use anyhow::Result;
use fs2::FileExt;
use neva::prelude::*;
use serde_json::json;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::info;

use crate::{
    config::Config,
    project::{self, ReadyTaskClaim, TaskLease},
    state::{machine::TaskState, store},
};

use super::tool_err;

pub const DESCRIPTION: &str = "Block until a task is ready to work on, then atomically claim it and return its full context. \
     Returns a JSON object: {\"status\":\"claimed\", \"claimed_by\":\"...\", \"lease_until\":\"...\", \
     \"state\":\"...\", \"task\":\"...\", \"review\":\"...\"} when a task is \
     claimed, or {\"status\":\"timeout\", \"state\":\"...\"} on timeout. \
     On timeout, inspect the state field — call wait_for_task again only if the state is \
     Executing or Addressing. \
     Each call waits up to `wait_timeout_secs` (see ferrus.toml), then returns timeout so the \
     agent can poll again. \
     Call this at the start of each Executor session; after a rejection, the next Executor \
     session should call it again to claim the Addressing work.";

pub async fn handler(agent_id: &str) -> Result<String, Error> {
    run(agent_id).await.map_err(tool_err)
}

async fn run(agent_id: &str) -> Result<String> {
    let config = Config::load().await?;
    let timeout = Duration::from_secs(config.limits.wait_timeout_secs);
    let ttl_secs = config.lease.ttl_secs;
    let start = Instant::now();

    loop {
        // Keep STATE.lock during the claim cycle so the compatibility STATE.json mirror
        // cannot race with the SQLite task queue claim.
        let claim = {
            let lock_file = store::open_lock_file()?;
            let lock_file = tokio::task::spawn_blocking(move || -> Result<std::fs::File> {
                lock_file.lock_exclusive().map_err(anyhow::Error::from)?;
                Ok(lock_file)
            })
            .await??;

            let mut state = store::read_state().await?;
            let claim = claim_ready_task(agent_id, ttl_secs, &mut state).await?;

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
            let task = store::read_task_at(&claim.task_path).await?;
            let run_dir = run_dir_for_task(&claim.task_id);
            let review = store::read_review_for_run_dir(&run_dir).await?;

            info!(agent_id, task_id = claim.task_id, "Executor claimed task");
            let response = json!({
                "status": "claimed",
                "task_id": claim.task_id,
                "task_path": claim.task_path,
                "run_dir": run_dir,
                "claimed_by": claim.claimed_by,
                "lease_until": claim.lease_until,
                "state": state_name_for_task_status(&claim.status),
                "task": task,
                "review": review,
                "check_retries_used": claim.check_retries,
                "review_cycles_used": claim.review_cycles,
            });
            return Ok(response.to_string());
        }

        if start.elapsed() >= timeout {
            let state = store::read_state().await?;
            info!("wait_for_task timed out, state: {:?}", state.state);
            let response = json!({
                "status": "timeout",
                "state": format!("{:?}", state.state),
            });
            return Ok(response.to_string());
        }

        sleep(Duration::from_millis(500)).await;
    }
}

async fn claim_ready_task(
    agent_id: &str,
    ttl_secs: u64,
    state: &mut crate::state::machine::StateData,
) -> Result<Option<TaskLease>> {
    match project::claim_next_ready_task(agent_id, ttl_secs).await {
        Ok(ReadyTaskClaim::Claimed(task)) | Ok(ReadyTaskClaim::AlreadyClaimed(task)) => {
            mirror_state_lease_if_current_task(state, &task).await?;
            Ok(Some(task))
        }
        Ok(ReadyTaskClaim::NoAvailable) => Ok(None),
        Err(err) => {
            tracing::warn!(
                error = ?err,
                "failed to claim next ready task in ferrus.db; falling back to STATE.json lease"
            );
            claim_state_fallback(agent_id, ttl_secs, state).await
        }
    }
}

async fn mirror_state_lease_if_current_task(
    state: &mut crate::state::machine::StateData,
    task: &TaskLease,
) -> Result<()> {
    if state.active_task_id.as_deref() == Some(task.task_id.as_str()) {
        state.claimed_by = Some(task.claimed_by.clone());
        state.lease_until = Some(task.lease_until);
        state.last_heartbeat = Some(chrono::Utc::now());
        store::write_state(state).await?;
    }
    Ok(())
}

async fn claim_state_fallback(
    agent_id: &str,
    ttl_secs: u64,
    state: &mut crate::state::machine::StateData,
) -> Result<Option<TaskLease>> {
    let claimable = matches!(state.state, TaskState::Executing | TaskState::Addressing);
    if !claimable {
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
        status: task_status_for_state(&state.state).to_string(),
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
            .ok_or_else(|| anyhow::anyhow!("claimed STATE.json task is missing lease_until"))?,
    }))
}

fn run_dir_for_task(task_id: &str) -> String {
    format!(".ferrus/runs/{task_id}")
}

fn state_name_for_task_status(status: &str) -> &'static str {
    match status {
        "addressing" => "Addressing",
        _ => "Executing",
    }
}

fn task_status_for_state(state: &TaskState) -> &'static str {
    match state {
        TaskState::Addressing => "addressing",
        _ => "executing",
    }
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
        std::fs::create_dir_all(dir.path().join(".ferrus/runs")).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        tokio::fs::write(
            "ferrus.toml",
            "[checks]\ncommands = []\n\n[limits]\nmax_check_retries = 20\nmax_review_cycles = 3\nmax_feedback_lines = 30\nwait_timeout_secs = 1\n\n[lease]\nttl_secs = 60\n",
        )
        .await
        .unwrap();
        tokio::fs::write(".ferrus/STATE.lock", "").await.unwrap();
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
    async fn wait_for_task_claims_next_ready_database_task() {
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
        tokio::fs::write(".ferrus/tasks/t-001.md", "first task")
            .await
            .unwrap();
        tokio::fs::write(".ferrus/tasks/t-002.md", "second task")
            .await
            .unwrap();
        crate::project::record_task_status("t-001", ".ferrus/tasks/t-001.md", "executing")
            .await
            .unwrap();
        crate::project::record_task_status("t-002", ".ferrus/tasks/t-002.md", "executing")
            .await
            .unwrap();
        crate::project::claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:1", 60)
            .await
            .unwrap();

        let response: serde_json::Value =
            serde_json::from_str(&run("executor:codex:2").await.unwrap()).unwrap();

        assert_eq!(response["status"], "claimed");
        assert_eq!(response["task_id"], "t-002");
        assert_eq!(response["task_path"], ".ferrus/tasks/t-002.md");
        assert_eq!(response["run_dir"], ".ferrus/runs/t-002");
        assert_eq!(response["task"], "second task");

        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-002").unwrap();
        assert_eq!(task.claimed_by.as_deref(), Some("executor:codex:2"));

        teardown(previous);
    }
}
