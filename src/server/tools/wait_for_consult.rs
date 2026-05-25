use anyhow::Result;
use neva::prelude::*;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::info;

use crate::{
    config::Config,
    project::{self, RuntimeTaskContext, TaskConsultRestore},
    state::{
        machine::{StateData, TaskState},
        store,
    },
};

use super::{
    ensure_lease_identity, ensure_lease_owner_or_reclaim,
    runtime_task_context_for_agent_best_effort, tool_err,
};

pub const DESCRIPTION: &str = "Block until CONSULT_RESPONSE.md exists, then restore the pre-consult state and \
     return the consultant's response text. Each call waits up to `wait_timeout_secs` and then \
     returns an error telling the agent to call /wait_for_consult again. Must only be called while state is Consultation.";

pub async fn handler_for_agent(agent_id: &str) -> Result<String, Error> {
    run(agent_id).await.map_err(tool_err)
}

async fn run(agent_id: &str) -> Result<String> {
    let config = Config::load().await?;
    let timeout = Duration::from_secs(config.limits.wait_timeout_secs);
    let start = Instant::now();

    let runtime_context = runtime_task_context_for_agent_best_effort(agent_id).await;
    let mut state = store::read_state().await.ok();
    let context_is_consultation = matches!(
        runtime_context
            .as_ref()
            .map(|context| context.status.as_str()),
        Some("consultation")
    );
    let state_is_consultation = state
        .as_ref()
        .is_some_and(|state| state.state == TaskState::Consultation);
    if !context_is_consultation && !state_is_consultation {
        let current_state = state
            .as_ref()
            .map(|state| format!("{:?}", state.state))
            .unwrap_or_else(|| "unavailable".to_string());
        anyhow::bail!(
            "Cannot wait for consultation from state {current_state}. Call /consult first.",
        );
    }
    let use_legacy_state = should_use_legacy_state(state.as_ref(), runtime_context.as_ref());
    if use_legacy_state {
        let state = state.as_ref().ok_or_else(|| {
            anyhow::anyhow!("Cannot wait for legacy consultation: STATE.json is missing")
        })?;
        ensure_lease_identity(state, agent_id)?;
    } else {
        let state_for_lease = state.get_or_insert_with(StateData::default);
        ensure_lease_owner_or_reclaim(state_for_lease, agent_id, config.lease.ttl_secs).await?;
    }

    loop {
        match read_consult_response(use_legacy_state, runtime_context.as_ref()).await {
            Ok(response) if !response.trim().is_empty() => {
                if use_legacy_state {
                    let mut state = store::read_state().await?;
                    ensure_lease_identity(&state, agent_id)?;
                    let resumed = state.finish_consult()?;
                    if let Some(agent_id) = state.claimed_by.clone() {
                        store::claim_state(&agent_id, config.lease.ttl_secs, &mut state).await?;
                    } else {
                        store::write_state(&state).await?;
                    }
                    store::clear_consult_response().await?;
                    store::clear_consult_request().await?;
                    if let Some(context) = runtime_context.as_ref() {
                        let _ = project::restore_task_from_consultation(&context.task_id).await?;
                        store::clear_consult_response_for_run_dir(&context.run_dir).await?;
                        store::clear_consult_request_for_run_dir(&context.run_dir).await?;
                    }

                    let response = response.trim().to_string();
                    info!(resumed = ?resumed, "Consultation answered; state restored");
                    return Ok(response);
                } else if let Some(context) = runtime_context.as_ref() {
                    let restored =
                        project::restore_task_from_consultation(&context.task_id).await?;
                    let resumed = match restored {
                        TaskConsultRestore::Restored { status } => status,
                        TaskConsultRestore::NotInConsultation => context.status.clone(),
                    };
                    store::clear_consult_response_for_run_dir(&context.run_dir).await?;
                    store::clear_consult_request_for_run_dir(&context.run_dir).await?;

                    let response = response.trim().to_string();
                    info!(
                        task_id = context.task_id,
                        resumed, "Consultation answered; DB task restored"
                    );
                    return Ok(response);
                } else {
                    anyhow::bail!("Cannot restore consultation without runtime task context");
                }
            }
            _ => {}
        }

        if start.elapsed() >= timeout {
            anyhow::bail!(
                "Timed out waiting for CONSULT_RESPONSE.md. Call /wait_for_consult again to keep waiting."
            );
        }

        sleep(Duration::from_millis(500)).await;
    }
}

async fn read_consult_response(
    use_legacy_state: bool,
    context: Option<&RuntimeTaskContext>,
) -> Result<String> {
    if let Some(context) = context {
        let scoped = store::read_consult_response_for_run_dir(&context.run_dir).await?;
        if !use_legacy_state || !scoped.trim().is_empty() {
            return Ok(scoped);
        }
    }
    store::read_consult_response().await
}

fn should_use_legacy_state(
    state: Option<&StateData>,
    context: Option<&RuntimeTaskContext>,
) -> bool {
    context.is_none()
        || state.is_some_and(|state| {
            context.is_some_and(|context| {
                state.active_task_id.as_deref() == Some(context.task_id.as_str())
            })
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
    async fn wait_for_consult_restores_agent_runtime_task_from_scoped_response() {
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
        crate::project::record_task_status("t-007", ".ferrus/tasks/t-007.md", "addressing")
            .await
            .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "executor:codex:7", 60)
            .await
            .unwrap();
        crate::project::record_task_consultation_requested("t-007", "addressing")
            .await
            .unwrap();
        store::write_consult_request_for_run_dir(".ferrus/runs/t-007", "question")
            .await
            .unwrap();
        store::write_consult_response_for_run_dir(".ferrus/runs/t-007", "answer\n")
            .await
            .unwrap();

        let response = run("executor:codex:7").await.unwrap();

        assert_eq!(response, "answer");
        let state = store::read_state().await.unwrap();
        assert_eq!(state.state, TaskState::Executing);
        assert_eq!(state.active_task_id.as_deref(), Some("t-001"));
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "addressing");
        assert_eq!(task.paused_status, None);
        assert_eq!(task.claimed_by.as_deref(), Some("executor:codex:7"));
        assert_eq!(
            store::read_consult_request_for_run_dir(".ferrus/runs/t-007")
                .await
                .unwrap(),
            ""
        );
        assert_eq!(
            store::read_consult_response_for_run_dir(".ferrus/runs/t-007")
                .await
                .unwrap(),
            ""
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn wait_for_consult_uses_database_context_when_state_json_is_absent() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status("t-007", ".ferrus/tasks/t-007.md", "addressing")
            .await
            .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "executor:codex:7", 60)
            .await
            .unwrap();
        crate::project::record_task_consultation_requested("t-007", "addressing")
            .await
            .unwrap();
        store::write_consult_response_for_run_dir(".ferrus/runs/t-007", "answer\n")
            .await
            .unwrap();

        let response = run("executor:codex:7").await.unwrap();

        assert_eq!(response, "answer");
        assert!(store::read_state().await.is_err());
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "addressing");
        assert_eq!(task.paused_status, None);
        assert_eq!(task.claimed_by.as_deref(), Some("executor:codex:7"));

        teardown(previous);
    }

    #[tokio::test]
    async fn wait_for_consult_restores_active_task_database_mirror_from_scoped_response() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        let mut state = StateData {
            state: TaskState::Consultation,
            paused_state: Some(TaskState::Addressing),
            claimed_by: Some("executor:codex:1".to_string()),
            lease_until: Some(chrono::Utc::now() + chrono::Duration::seconds(60)),
            ..StateData::default()
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
        crate::project::record_task_consultation_requested("t-001", "addressing")
            .await
            .unwrap();
        store::write_consult_response_for_run_dir(".ferrus/runs/t-001", "answer\n")
            .await
            .unwrap();

        let response = run("executor:codex:1").await.unwrap();

        assert_eq!(response, "answer");
        let state = store::read_state().await.unwrap();
        assert_eq!(state.state, TaskState::Addressing);
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-001").unwrap();
        assert_eq!(task.status, "addressing");
        assert_eq!(task.paused_status, None);
        assert_eq!(
            store::read_consult_response_for_run_dir(".ferrus/runs/t-001")
                .await
                .unwrap(),
            ""
        );

        teardown(previous);
    }
}
