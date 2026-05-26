use anyhow::Result;
use neva::prelude::*;
use std::path::Path;
use tokio::process::Command;
use tracing::info;

use crate::{
    agent_id::ENV_PROJECT_ROOT,
    config::Config,
    project::{self, RuntimeTaskContext, TaskCheckFailure},
    state::{
        machine::{StateData, TaskState, TransitionError},
        store,
    },
};

use super::{
    check_gate::{self, CheckGateResult},
    ensure_lease_owner_or_reclaim, runtime_task_context_for_agent_best_effort, tool_err,
    uses_legacy_state_context,
};

pub const DESCRIPTION: &str = "\
Run the final check gate and, if it passes, submit work for Supervisor review. \
Can be called from Executing or Addressing. \
On pass: state → Reviewing. On fail: stay in the current work state (or state \
→ Failed if the retry limit is exhausted).

The `content` parameter must be a Markdown document with the following sections:

## Summary
Brief description of what was changed and why.

## How to verify manually
Step-by-step instructions for the Supervisor to spot-check the work.

## Known limitations
Anything deliberately left out, edge cases not handled, or follow-up work needed. \
Omit this section if there are none.";

pub const INPUT_SCHEMA: &str = r#"{
    "properties": {
        "content": {
            "type": "string",
            "description": "Submission notes in Markdown (summary, how to verify, known limitations)"
        }
    },
    "required": ["content"]
}"#;

pub async fn handler_for_agent(agent_id: &str, content: String) -> Result<String, Error> {
    run(Some(agent_id), content).await.map_err(tool_err)
}

async fn run(agent_id: Option<&str>, content: String) -> Result<String> {
    let config = Config::load().await?;
    let runtime_context = runtime_context(agent_id).await;
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
            "Cannot submit from state {current_state}. Submit is only valid from Executing or Addressing after the implementation is ready.",
        );
    }
    if let Some(agent_id) = agent_id {
        let state_for_lease = state.get_or_insert_with(StateData::default);
        ensure_lease_owner_or_reclaim(state_for_lease, agent_id, config.lease.ttl_secs).await?;
    }
    let use_legacy_state = uses_legacy_state_context(state.as_ref(), runtime_context.as_ref());

    if config.checks.commands.is_empty() {
        info!("No check commands configured; treating final check gate as pass");
        if use_legacy_state {
            let state = state.as_mut().ok_or_else(|| {
                anyhow::anyhow!("Cannot submit legacy state: STATE.json is missing")
            })?;
            state.check_passed()?;
            state.submit()?;
        } else if let Some(context) = runtime_context.as_ref() {
            project::record_task_check_passed(&context.task_id).await?;
        }
        write_submission(state.as_ref(), runtime_context.as_ref(), &content).await?;
        write_submission_patch(runtime_context.as_ref()).await?;
        if use_legacy_state {
            let state = state.as_ref().ok_or_else(|| {
                anyhow::anyhow!("Cannot submit legacy state: STATE.json is missing")
            })?;
            store::write_state(state).await?;
            mirror_check_state(runtime_context.as_ref(), state).await?;
        }
        record_task_status(runtime_context.as_ref(), "reviewing").await;
        project::record_runtime_event_best_effort(
            runtime_context
                .as_ref()
                .and_then(|context| context.run_id.clone()),
            "submitted",
            serde_json::json!({ "content_bytes": content.len(), "check_gate": "skipped" }),
        )
        .await;

        return Ok(
            "Submitted for review. Warning: no check commands are configured in ferrus.toml, so the final check gate was treated as a pass. State: Reviewing."
                .to_string(),
        );
    }

    info!("Running final check gate before review submission");
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
                    anyhow::anyhow!("Cannot submit legacy state: STATE.json is missing")
                })?;
                state.check_passed()?;
                state.submit()?;
            } else if let Some(context) = runtime_context.as_ref() {
                project::record_task_check_passed(&context.task_id).await?;
            }
            write_submission(state.as_ref(), runtime_context.as_ref(), &content).await?;
            write_submission_patch(runtime_context.as_ref()).await?;
            if use_legacy_state {
                let state = state.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("Cannot submit legacy state: STATE.json is missing")
                })?;
                store::write_state(state).await?;
                mirror_check_state(runtime_context.as_ref(), state).await?;
            }
            record_task_status(runtime_context.as_ref(), "reviewing").await;
            project::record_runtime_event_best_effort(
                runtime_context
                    .as_ref()
                    .and_then(|context| context.run_id.clone()),
                "submitted",
                serde_json::json!({ "content_bytes": content.len(), "check_gate": "passed" }),
            )
            .await;

            info!("Work submitted for review, state → Reviewing");
            Ok(
                "Submitted for review. State: Reviewing. The Supervisor can now call /review_pending."
                    .to_string(),
            )
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
                            "submit_check_failed",
                            serde_json::json!({
                                "task_id": context.task_id,
                                "retries": retries,
                                "max_retries": config.limits.max_check_retries,
                                "state": context.status,
                            }),
                        )
                        .await;
                        Ok(format!(
                            "Final review gate failed during /submit (retry {}/{}).\n\n{}\n\nState remains {}. Fix the issues and run /check or /submit again.",
                            retries,
                            config.limits.max_check_retries,
                            failure.report,
                            context.status,
                        ))
                    }
                    TaskCheckFailure::LimitExceeded { retries } => {
                        project::record_runtime_event_best_effort(
                            context.run_id.clone(),
                            "submit_check_limit_exceeded",
                            serde_json::json!({
                                "task_id": context.task_id,
                                "retries": retries,
                                "max_retries": config.limits.max_check_retries,
                            }),
                        )
                        .await;
                        Ok(format!(
                            "Final review gate failed during /submit and hit the retry limit ({retries}/{}).\n\n{}\n\nState is now Failed. A human must call /reset to recover.",
                            config.limits.max_check_retries, failure.report,
                        ))
                    }
                };
            }
            let state = state.as_mut().ok_or_else(|| {
                anyhow::anyhow!("Cannot submit legacy state: STATE.json is missing")
            })?;
            match state.check_failed(failure.failure_reason, config.limits.max_check_retries) {
                Ok(()) => {
                    store::write_state(state).await?;
                    mirror_check_state(runtime_context.as_ref(), state).await?;
                    project::record_runtime_event_best_effort(
                        None,
                        "submit_check_failed",
                        serde_json::json!({
                            "retries": state.check_retries,
                            "max_retries": config.limits.max_check_retries,
                            "state": format!("{:?}", state.state),
                        }),
                    )
                    .await;
                    Ok(format!(
                        "Final review gate failed during /submit (retry {}/{}).\n\n{}\n\nState remains {:?}. Fix the issues and run /check or /submit again.",
                        state.check_retries,
                        config.limits.max_check_retries,
                        failure.report,
                        state.state,
                    ))
                }
                Err(TransitionError::CheckLimitExceeded { retries }) => {
                    store::write_state(state).await?;
                    mirror_check_state(runtime_context.as_ref(), state).await?;
                    record_task_status(runtime_context.as_ref(), "failed").await;
                    project::record_runtime_event_best_effort(
                        runtime_context
                            .as_ref()
                            .and_then(|context| context.run_id.clone()),
                        "submit_check_limit_exceeded",
                        serde_json::json!({
                            "retries": retries,
                            "max_retries": config.limits.max_check_retries,
                        }),
                    )
                    .await;
                    Ok(format!(
                        "Final review gate failed during /submit and hit the retry limit ({retries}/{}).\n\n{}\n\nState is now Failed. A human must call /reset to recover.",
                        config.limits.max_check_retries, failure.report,
                    ))
                }
                Err(e) => anyhow::bail!(e),
            }
        }
    }
}

async fn runtime_context(agent_id: Option<&str>) -> Option<RuntimeTaskContext> {
    match agent_id {
        Some(agent_id) => runtime_task_context_for_agent_best_effort(agent_id).await,
        None => None,
    }
}

async fn write_submission(
    _state: Option<&StateData>,
    context: Option<&RuntimeTaskContext>,
    content: &str,
) -> Result<()> {
    if let Some(context) = context {
        store::write_submission_for_run_dir(&context.run_dir, content).await?;
        return Ok(());
    }
    store::write_submission(content).await
}

async fn write_submission_patch(context: Option<&RuntimeTaskContext>) -> Result<()> {
    let Some(context) = context else {
        return Ok(());
    };
    if !is_isolated_executor_workspace(context).await {
        return Ok(());
    }

    let patch = workspace_patch().await?;
    store::write_patch_for_run_dir(&context.run_dir, &patch).await
}

async fn is_isolated_executor_workspace(context: &RuntimeTaskContext) -> bool {
    let Some(project_root) = std::env::var(ENV_PROJECT_ROOT)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    let current_dir = std::env::current_dir().ok();
    let workspace_path = context
        .workspace_path
        .as_deref()
        .map(Path::new)
        .map(|path| path.to_path_buf())
        .or(current_dir);
    let Some(workspace_path) = workspace_path else {
        return false;
    };
    !equivalent_paths(&workspace_path, Path::new(&project_root)).await
}

async fn equivalent_paths(left: &Path, right: &Path) -> bool {
    let left = tokio::fs::canonicalize(left)
        .await
        .unwrap_or_else(|_| left.to_path_buf());
    let right = tokio::fs::canonicalize(right)
        .await
        .unwrap_or_else(|_| right.to_path_buf());
    left == right
}

async fn workspace_patch() -> Result<String> {
    let _ = Command::new("git").args(["add", "-N", "."]).output().await;
    let output = Command::new("git")
        .args(["diff", "--binary", "HEAD", "--"])
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "Failed to capture executor workspace patch: {}",
            if stderr.is_empty() {
                output.status.to_string()
            } else {
                stderr
            }
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn record_task_status(context: Option<&RuntimeTaskContext>, status: &str) {
    if let Some(context) = context {
        project::record_task_status_best_effort(&context.task_id, &context.task_path, status).await;
    } else {
        project::record_current_task_status_best_effort(status).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::machine::StateData;
    use chrono::Utc;
    use tempfile::TempDir;

    async fn setup() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ferrus")).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        tokio::fs::write(
            "ferrus.toml",
            "[checks]\ncommands = []\n\n[limits]\nmax_check_retries = 20\nmax_review_cycles = 3\nmax_feedback_lines = 30\nwait_timeout_secs = 60\n",
        )
        .await
        .unwrap();
        (dir, previous)
    }

    fn teardown(previous: std::path::PathBuf) {
        std::env::set_current_dir(previous).unwrap();
    }

    #[tokio::test]
    async fn submit_reclaims_expired_same_agent_lease_before_guarding() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        let mut state = StateData {
            state: TaskState::Executing,
            claimed_by: Some("executor:codex:1".to_string()),
            lease_until: Some(Utc::now() - chrono::Duration::seconds(1)),
            last_heartbeat: Some(Utc::now() - chrono::Duration::seconds(2)),
            ..StateData::default()
        };
        state.set_active_task_artifacts(
            "t-001".to_string(),
            ".ferrus/tasks/t-001.md".to_string(),
            ".ferrus/runs/t-001".to_string(),
        );
        store::write_state(&state).await.unwrap();

        run(
            Some("executor:codex:1"),
            "## Summary\nDone.\n\n## How to verify manually\nInspect it.\n".to_string(),
        )
        .await
        .unwrap();

        let state = store::read_state().await.unwrap();
        assert_eq!(state.state, TaskState::Reviewing);
        assert!(state.claimed_by.is_none());
        assert!(state.lease_until.is_none());
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/runs/t-001/SUBMISSION.md")
                .await
                .unwrap(),
            "## Summary\nDone.\n\n## How to verify manually\nInspect it.\n"
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn submit_pass_prefers_database_context_over_active_state_mirror() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (dir, previous) = setup().await;
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
        let mut state = StateData {
            state: TaskState::Executing,
            check_retries: 1,
            failure_reason: Some("fmt failed".to_string()),
            claimed_by: Some("executor:codex:1".to_string()),
            lease_until: Some(Utc::now() + chrono::Duration::seconds(60)),
            last_heartbeat: Some(Utc::now()),
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

        run(
            Some("executor:codex:1"),
            "## Summary\nDone.\n\n## How to verify manually\nInspect it.\n".to_string(),
        )
        .await
        .unwrap();

        let state = store::read_state().await.unwrap();
        assert_eq!(state.state, TaskState::Executing);
        assert_eq!(state.check_retries, 1);
        assert_eq!(state.failure_reason.as_deref(), Some("fmt failed"));
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-001").unwrap();
        assert_eq!(task.status, "reviewing");
        assert_eq!(task.check_retries, 0);
        assert_eq!(task.failure_reason, None);
        assert_eq!(task.claimed_by, None);

        teardown(previous);
    }

    #[tokio::test]
    async fn submit_writes_submission_to_agent_runtime_task_context() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (dir, previous) = setup().await;
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
        crate::project::record_task_status("t-007", ".ferrus/tasks/t-007.md", "executing")
            .await
            .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "executor:codex:7", 60)
            .await
            .unwrap();
        let mut state = StateData {
            state: TaskState::Executing,
            claimed_by: Some("executor:codex:1".to_string()),
            lease_until: Some(Utc::now() + chrono::Duration::seconds(60)),
            last_heartbeat: Some(Utc::now()),
            ..StateData::default()
        };
        state.set_active_task_artifacts(
            "t-001".to_string(),
            ".ferrus/tasks/t-001.md".to_string(),
            ".ferrus/runs/t-001".to_string(),
        );
        store::write_state(&state).await.unwrap();

        run(
            Some("executor:codex:7"),
            "## Summary\nDone.\n\n## How to verify manually\nInspect it.\n".to_string(),
        )
        .await
        .unwrap();

        assert_eq!(
            tokio::fs::read_to_string(".ferrus/runs/t-007/SUBMISSION.md")
                .await
                .unwrap(),
            "## Summary\nDone.\n\n## How to verify manually\nInspect it.\n"
        );
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "reviewing");
        assert_eq!(task.check_retries, 0);
        assert_eq!(task.claimed_by, None);
        let state = store::read_state().await.unwrap();
        assert_eq!(state.state, TaskState::Executing);
        assert_eq!(state.active_task_id.as_deref(), Some("t-001"));

        teardown(previous);
    }

    #[tokio::test]
    async fn submit_uses_database_context_when_state_json_is_absent() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (dir, previous) = setup().await;
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
        crate::project::record_task_status("t-007", ".ferrus/tasks/t-007.md", "executing")
            .await
            .unwrap();
        crate::project::record_task_check_failed("t-007", "fmt failed", 2)
            .await
            .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "executor:codex:7", 60)
            .await
            .unwrap();

        run(
            Some("executor:codex:7"),
            "## Summary\nDone.\n\n## How to verify manually\nInspect it.\n".to_string(),
        )
        .await
        .unwrap();

        assert!(store::read_state().await.is_err());
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/runs/t-007/SUBMISSION.md")
                .await
                .unwrap(),
            "## Summary\nDone.\n\n## How to verify manually\nInspect it.\n"
        );
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "reviewing");
        assert_eq!(task.check_retries, 0);
        assert_eq!(task.failure_reason, None);
        assert_eq!(task.claimed_by, None);

        teardown(previous);
    }
}
