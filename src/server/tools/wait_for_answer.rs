use anyhow::Result;
use neva::prelude::*;
use serde_json::json;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::info;

use crate::{
    config::Config,
    project::{self, RuntimeTaskContext, TaskHumanAnswerRestore},
    state::store,
};

use super::{ensure_lease_owner_or_reclaim, require_runtime_task_context, tool_err};

pub const DESCRIPTION: &str = "Block until the human provides an answer to the question you asked via /ask_human. \
     Polls .ferrus/ANSWER.md until it has content, then restores the paused state and \
     returns the answer. \
     Returns {\"status\":\"answered\", \"answer\":\"...\", \"resumed_state\":\"...\"} on success, \
     or {\"status\":\"timeout\"} if `wait_timeout_secs` elapses for this call. On timeout, call this tool again \
     to keep waiting. \
     Must only be called immediately after /ask_human while state is AwaitingHuman.";

pub async fn handler_for_agent(agent_id: &str) -> Result<String, Error> {
    run(agent_id).await.map_err(tool_err)
}

async fn run(agent_id: &str) -> Result<String> {
    let config = Config::load().await?;
    let context = require_runtime_task_context(agent_id).await?;
    if context.status != "awaiting_human" {
        anyhow::bail!(
            "Cannot wait for answer from task status {:?}; expected awaiting_human",
            context.status
        );
    }
    ensure_scoped_answer_waiter(&context, agent_id, config.lease.ttl_secs).await?;

    let timeout = Duration::from_secs(config.limits.wait_timeout_secs);
    let start = Instant::now();

    loop {
        match read_answer(&context).await {
            Ok(ans) if !ans.trim().is_empty() => {
                let restored = project::restore_task_from_human_answer(&context.task_id).await?;
                let resumed = match restored {
                    TaskHumanAnswerRestore::Restored { status } => status,
                    TaskHumanAnswerRestore::NotAwaitingHuman => context.status.clone(),
                };
                store::clear_answer_for_run_dir(&context.run_dir).await?;
                store::clear_question_for_run_dir(&context.run_dir).await?;

                let answer = ans.trim().to_string();
                info!(resumed, "Human answered; task restored");
                let response = json!({
                    "status": "answered",
                    "answer": answer,
                    "resumed_state": resumed,
                });
                return Ok(response.to_string());
            }
            _ => {}
        }

        if start.elapsed() >= timeout {
            info!("wait_for_answer timed out");
            let response = json!({"status": "timeout"});
            return Ok(response.to_string());
        }

        sleep(Duration::from_secs(2)).await;
    }
}

async fn ensure_scoped_answer_waiter(
    context: &RuntimeTaskContext,
    agent_id: &str,
    ttl_secs: u64,
) -> Result<()> {
    if let Some(owner) = project::task_human_question_owner(&context.task_id).await? {
        if owner == agent_id {
            return Ok(());
        }
        anyhow::bail!("Cannot wait for answer: question was asked by {owner}, not {agent_id}");
    }
    ensure_lease_owner_or_reclaim(agent_id, ttl_secs).await
}

async fn read_answer(context: &RuntimeTaskContext) -> Result<String> {
    store::read_answer_for_run_dir(&context.run_dir).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{machine::TaskState, store};
    use chrono::Utc;
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
    async fn wait_for_answer_restores_scoped_runtime_task_without_touching_active_state() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        let mut state = crate::state::machine::StateData {
            state: TaskState::Executing,
            claimed_by: Some("executor:codex:1".to_string()),
            lease_until: Some(Utc::now() + chrono::Duration::seconds(60)),
            ..Default::default()
        };
        state.set_active_task_artifacts(
            "t-001".to_string(),
            ".ferrus/tasks/t-001.md".to_string(),
            ".ferrus/runs/t-001".to_string(),
        );
        store::write_state(&state).await.unwrap();
        crate::project::record_task_status("t-007", ".ferrus/tasks/t-007.md", "executing")
            .await
            .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "executor:codex:7", 60)
            .await
            .unwrap();
        crate::project::record_task_human_question_requested(
            "t-007",
            "executing",
            "executor:codex:7",
        )
        .await
        .unwrap();
        store::write_answer_for_run_dir(".ferrus/runs/t-007", "Use the stable path.")
            .await
            .unwrap();

        let response = run("executor:codex:7").await.unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();

        assert_eq!(response["status"], "answered");
        assert_eq!(response["answer"], "Use the stable path.");
        assert_eq!(response["resumed_state"], "executing");
        let state = store::read_state().await.unwrap();
        assert_eq!(state.state, TaskState::Executing);
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "executing");
        assert_eq!(task.paused_status, None);
        assert_eq!(
            store::read_answer_for_run_dir(".ferrus/runs/t-007")
                .await
                .unwrap(),
            ""
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn wait_for_answer_uses_database_context_when_state_json_is_absent() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status("t-007", ".ferrus/tasks/t-007.md", "executing")
            .await
            .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "executor:codex:7", 60)
            .await
            .unwrap();
        crate::project::record_task_human_question_requested(
            "t-007",
            "executing",
            "executor:codex:7",
        )
        .await
        .unwrap();
        store::write_answer_for_run_dir(".ferrus/runs/t-007", "Use the stable path.")
            .await
            .unwrap();

        let response = run("executor:codex:7").await.unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();

        assert_eq!(response["status"], "answered");
        assert_eq!(response["answer"], "Use the stable path.");
        assert_eq!(response["resumed_state"], "executing");
        assert!(store::read_state().await.is_err());
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "executing");
        assert_eq!(task.paused_status, None);
        assert_eq!(
            crate::project::task_human_question_owner("t-007")
                .await
                .unwrap(),
            None
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn wait_for_answer_prefers_database_context_over_active_state_mirror() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        let mut state = crate::state::machine::StateData {
            state: TaskState::AwaitingHuman,
            paused_state: Some(TaskState::Addressing),
            awaiting_human_by: Some("executor:codex:1".to_string()),
            claimed_by: Some("executor:codex:1".to_string()),
            lease_until: Some(Utc::now() + chrono::Duration::seconds(60)),
            ..Default::default()
        };
        state.set_active_task_artifacts(
            "t-001".to_string(),
            ".ferrus/tasks/t-001.md".to_string(),
            ".ferrus/runs/t-001".to_string(),
        );
        store::write_state(&state).await.unwrap();
        crate::project::record_task_status("t-001", ".ferrus/tasks/t-001.md", "addressing")
            .await
            .unwrap();
        crate::project::claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:1", 60)
            .await
            .unwrap();
        crate::project::record_task_human_question_requested(
            "t-001",
            "addressing",
            "executor:codex:1",
        )
        .await
        .unwrap();
        store::write_answer_for_run_dir(".ferrus/runs/t-001", "Use the stable path.")
            .await
            .unwrap();

        let response = run("executor:codex:1").await.unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();

        assert_eq!(response["status"], "answered");
        assert_eq!(response["answer"], "Use the stable path.");
        assert_eq!(response["resumed_state"], "addressing");
        let state = store::read_state().await.unwrap();
        assert_eq!(state.state, TaskState::AwaitingHuman);
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-001").unwrap();
        assert_eq!(task.status, "addressing");
        assert_eq!(task.paused_status, None);
        assert_eq!(
            store::read_answer_for_run_dir(".ferrus/runs/t-001")
                .await
                .unwrap(),
            ""
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn wait_for_answer_rejects_scoped_agent_that_did_not_ask_question() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        store::write_state(&crate::state::machine::StateData::default())
            .await
            .unwrap();
        crate::project::record_task_status("t-007", ".ferrus/tasks/t-007.md", "consultation")
            .await
            .unwrap();
        crate::project::record_run_started("supervisor", "supervisor:codex:7", std::process::id())
            .await
            .unwrap();
        crate::project::attach_running_run_to_task(
            "supervisor:codex:7",
            "t-007",
            ".ferrus/tasks/t-007.md",
        )
        .await
        .unwrap();
        crate::project::record_task_human_question_requested(
            "t-007",
            "consultation",
            "supervisor:codex:1",
        )
        .await
        .unwrap();
        store::write_answer_for_run_dir(".ferrus/runs/t-007", "Use the stable path.")
            .await
            .unwrap();

        let error = run("supervisor:codex:7").await.unwrap_err().to_string();

        assert!(error.contains("question was asked by supervisor:codex:1"));
        assert_eq!(
            store::read_answer_for_run_dir(".ferrus/runs/t-007")
                .await
                .unwrap(),
            "Use the stable path."
        );

        teardown(previous);
    }
}
