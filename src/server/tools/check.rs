use anyhow::Result;
use neva::prelude::*;
use tracing::{info, warn};

use crate::{
    config::Config,
    project::{self, RuntimeTaskContext, TaskCheckFailure},
    state::{
        machine::{StateData, TaskState, TransitionError},
        store,
    },
};

use super::{
    check_gate::{self, CheckGateResult},
    ensure_lease_owner_or_reclaim, tool_err, uses_legacy_state_context,
};

pub const DESCRIPTION: &str = "Run all configured checks (clippy, fmt, tests, etc.) against the current \
     codebase. Can be called from state Executing or Addressing. \
     On pass: stay in the current work state and clear check-failure metadata. \
     On fail: stay in the current work state (or state → Failed if the retry \
     limit is exhausted).";

pub async fn handler() -> Result<String, Error> {
    run(None).await.map_err(tool_err)
}

pub async fn handler_for_agent(agent_id: &str) -> Result<String, Error> {
    run(Some(agent_id)).await.map_err(tool_err)
}

async fn run(agent_id: Option<&str>) -> Result<String> {
    let config = Config::load().await?;
    let runtime_context = match agent_id {
        Some(agent_id) => super::runtime_task_context_for_agent_best_effort(agent_id).await,
        None => None,
    };
    let mut state = store::read_state().await.ok();

    let context_is_working = matches!(
        runtime_context
            .as_ref()
            .map(|context| context.status.as_str()),
        Some("executing" | "addressing")
    );
    let state_is_working = state
        .as_ref()
        .is_some_and(|state| matches!(state.state, TaskState::Executing | TaskState::Addressing));
    if !context_is_working && !state_is_working {
        let current_state = state
            .as_ref()
            .map(|state| format!("{:?}", state.state))
            .unwrap_or_else(|| "unavailable".to_string());
        anyhow::bail!(
            "Cannot run checks from state {current_state}. \
             Checks are only valid in Executing or Addressing state.",
        );
    }
    if let Some(agent_id) = agent_id {
        let state_for_lease = state.get_or_insert_with(StateData::default);
        ensure_lease_owner_or_reclaim(state_for_lease, agent_id, config.lease.ttl_secs).await?;
    }
    let use_legacy_state = uses_legacy_state_context(state.as_ref(), runtime_context.as_ref());

    if config.checks.commands.is_empty() {
        if use_legacy_state {
            let state = state.as_mut().ok_or_else(|| {
                anyhow::anyhow!("Cannot check legacy state: STATE.json is missing")
            })?;
            state.check_passed()?;
            store::write_state(state).await?;
            mirror_check_state(runtime_context.as_ref(), state).await?;
        } else if let Some(context) = runtime_context.as_ref() {
            project::record_task_check_passed(&context.task_id).await?;
        }
        project::record_runtime_event_best_effort(
            runtime_context
                .as_ref()
                .and_then(|context| context.run_id.clone()),
            "check_passed",
            serde_json::json!({ "commands": 0 }),
        )
        .await;
        info!("No check commands configured; treating /check as pass");
        return Ok(
            "All checks passed. Warning: no check commands are configured in ferrus.toml. State remains unchanged."
                .to_string(),
        );
    }

    info!("Running {} check(s)", config.checks.commands.len());
    let attempt = if use_legacy_state {
        state
            .as_ref()
            .map(|state| state.check_retries + 1)
            .unwrap_or(1)
    } else {
        runtime_context
            .as_ref()
            .map(|context| context.check_retries + 1)
            .unwrap_or(1)
    };
    match check_gate::run(&config, attempt).await? {
        CheckGateResult::Passed => {
            if use_legacy_state {
                let state = state.as_mut().ok_or_else(|| {
                    anyhow::anyhow!("Cannot check legacy state: STATE.json is missing")
                })?;
                state.check_passed()?;
                store::write_state(state).await?;
                mirror_check_state(runtime_context.as_ref(), state).await?;
            } else if let Some(context) = runtime_context.as_ref() {
                project::record_task_check_passed(&context.task_id).await?;
            }
            project::record_runtime_event_best_effort(
                runtime_context
                    .as_ref()
                    .and_then(|context| context.run_id.clone()),
                "check_passed",
                serde_json::json!({ "commands": config.checks.commands.len() }),
            )
            .await;
            let state_label = work_state_label(state.as_ref(), runtime_context.as_ref());
            info!(
                state = state_label,
                "All checks passed; staying in current work state"
            );
            Ok(format!(
                "All checks passed. State remains {state_label}. Continue working or call /submit when the task is ready for review."
            ))
        }
        CheckGateResult::Failed(failure) => {
            if !use_legacy_state {
                let Some(context) = runtime_context.as_ref() else {
                    anyhow::bail!("Cannot update task check failure without runtime task context");
                };
                return match project::record_task_check_failed(
                    &context.task_id,
                    &failure.failure_reason,
                    config.limits.max_check_retries,
                )
                .await?
                {
                    TaskCheckFailure::Failed { retries } => {
                        project::record_runtime_event_best_effort(
                            context.run_id.clone(),
                            "check_failed",
                            serde_json::json!({
                                "task_id": context.task_id,
                                "retries": retries,
                                "max_retries": config.limits.max_check_retries,
                                "state": context.status,
                            }),
                        )
                        .await;
                        warn!(
                            retries,
                            task_id = context.task_id,
                            "Checks failed; DB task stays in current work state"
                        );
                        Ok(format!(
                            "Checks failed (retry {}/{}).\n\n{}\n\nState remains {}. Fix the issues and call /check again.",
                            retries,
                            config.limits.max_check_retries,
                            failure.report,
                            context.status,
                        ))
                    }
                    TaskCheckFailure::LimitExceeded { retries } => {
                        project::record_runtime_event_best_effort(
                            context.run_id.clone(),
                            "check_limit_exceeded",
                            serde_json::json!({
                                "task_id": context.task_id,
                                "retries": retries,
                                "max_retries": config.limits.max_check_retries,
                            }),
                        )
                        .await;
                        warn!(
                            retries,
                            task_id = context.task_id,
                            "Check retry limit reached, DB task → failed"
                        );
                        Ok(format!(
                            "Check retry limit reached ({retries}/{}).\n\n{}\n\nState is now Failed. A human must call /reset to recover.",
                            config.limits.max_check_retries, failure.report,
                        ))
                    }
                };
            }
            let state = state.as_mut().ok_or_else(|| {
                anyhow::anyhow!("Cannot check legacy state: STATE.json is missing")
            })?;
            match state.check_failed(failure.failure_reason, config.limits.max_check_retries) {
                Ok(()) => {
                    store::write_state(state).await?;
                    mirror_check_state(runtime_context.as_ref(), state).await?;
                    project::record_runtime_event_best_effort(
                        None,
                        "check_failed",
                        serde_json::json!({
                            "retries": state.check_retries,
                            "max_retries": config.limits.max_check_retries,
                            "state": format!("{:?}", state.state),
                        }),
                    )
                    .await;
                    warn!(
                        retries = state.check_retries,
                        state = ?state.state,
                        "Checks failed; staying in current work state"
                    );
                    Ok(format!(
                        "Checks failed (retry {}/{}).\n\n{}\n\nState remains {:?}. Fix the issues and call /check again.",
                        state.check_retries,
                        config.limits.max_check_retries,
                        failure.report,
                        state.state,
                    ))
                }
                Err(TransitionError::CheckLimitExceeded { retries }) => {
                    store::write_state(state).await?;
                    mirror_check_state(runtime_context.as_ref(), state).await?;
                    project::record_current_task_status_best_effort("failed").await;
                    project::record_runtime_event_best_effort(
                        None,
                        "check_limit_exceeded",
                        serde_json::json!({
                            "retries": retries,
                            "max_retries": config.limits.max_check_retries,
                        }),
                    )
                    .await;
                    warn!(retries, "Check retry limit reached, state → Failed");
                    Ok(format!(
                        "Check retry limit reached ({retries}/{}).\n\n{}\n\nState is now Failed. A human must call /reset to recover.",
                        config.limits.max_check_retries, failure.report,
                    ))
                }
                Err(e) => anyhow::bail!(e),
            }
        }
    }
}

async fn mirror_check_state(
    context: Option<&RuntimeTaskContext>,
    state: &crate::state::machine::StateData,
) -> Result<()> {
    if let Some(context) = context {
        project::mirror_task_check_state(
            &context.task_id,
            project::task_status_for_state(&state.state),
            state.check_retries,
            state.failure_reason.as_deref(),
        )
        .await?;
    }
    Ok(())
}

fn work_state_label(state: Option<&StateData>, context: Option<&RuntimeTaskContext>) -> String {
    if let Some(context) = context {
        return context.status.clone();
    }
    state
        .map(|state| format!("{:?}", state.state))
        .unwrap_or_else(|| "unavailable".to_string())
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
            "[checks]\ncommands = []\n\n[limits]\nmax_check_retries = 2\nmax_review_cycles = 3\nmax_feedback_lines = 30\nwait_timeout_secs = 1\n\n[lease]\nttl_secs = 60\n",
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
    async fn check_pass_mirrors_active_state_counters_to_database_task() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        let mut state = StateData {
            state: TaskState::Executing,
            check_retries: 1,
            failure_reason: Some("fmt failed".to_string()),
            ..StateData::default()
        };
        state.set_active_task_artifacts(
            "t-001".to_string(),
            ".ferrus/tasks/t-001.md".to_string(),
            ".ferrus/runs/t-001".to_string(),
        );
        store::write_state(&state).await.unwrap();
        crate::project::record_task_status("t-001", ".ferrus/tasks/t-001.md", "executing")
            .await
            .unwrap();
        crate::project::record_task_check_failed("t-001", "fmt failed", 2)
            .await
            .unwrap();
        crate::project::claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:1", 60)
            .await
            .unwrap();

        run(Some("executor:codex:1")).await.unwrap();

        let state = store::read_state().await.unwrap();
        assert_eq!(state.check_retries, 0);
        assert_eq!(state.failure_reason, None);
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-001").unwrap();
        assert_eq!(task.status, "executing");
        assert_eq!(task.check_retries, 0);
        assert_eq!(task.failure_reason, None);
        assert_eq!(task.claimed_by.as_deref(), Some("executor:codex:1"));

        teardown(previous);
    }

    #[tokio::test]
    async fn check_uses_database_context_when_state_json_is_absent() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status("t-007", ".ferrus/tasks/t-007.md", "executing")
            .await
            .unwrap();
        crate::project::record_task_check_failed("t-007", "fmt failed", 2)
            .await
            .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "executor:codex:7", 60)
            .await
            .unwrap();

        run(Some("executor:codex:7")).await.unwrap();

        assert!(store::read_state().await.is_err());
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "executing");
        assert_eq!(task.check_retries, 0);
        assert_eq!(task.failure_reason, None);
        assert_eq!(task.claimed_by.as_deref(), Some("executor:codex:7"));

        teardown(previous);
    }
}
