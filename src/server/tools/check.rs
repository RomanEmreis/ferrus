use anyhow::Result;
use neva::prelude::*;
use tracing::{info, warn};

use crate::{
    config::Config,
    project::{self, RuntimeTaskContext, TaskCheckFailure},
    state::{
        machine::{TaskState, TransitionError},
        store,
    },
};

use super::{
    check_gate::{self, CheckGateResult},
    ensure_lease_owner_or_reclaim, tool_err,
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
    let mut state = store::read_state().await?;
    let runtime_context = match agent_id {
        Some(agent_id) => super::runtime_task_context_for_agent_best_effort(agent_id).await,
        None => None,
    };

    if !matches!(state.state, TaskState::Executing | TaskState::Addressing)
        && !matches!(
            runtime_context
                .as_ref()
                .map(|context| context.status.as_str()),
            Some("executing" | "addressing")
        )
    {
        anyhow::bail!(
            "Cannot run checks from state {other:?}. \
             Checks are only valid in Executing or Addressing state.",
            other = state.state
        );
    }
    if let Some(agent_id) = agent_id {
        ensure_lease_owner_or_reclaim(&mut state, agent_id, config.lease.ttl_secs).await?;
    }
    let use_legacy_state = should_use_legacy_state(&state, runtime_context.as_ref());

    if config.checks.commands.is_empty() {
        if use_legacy_state {
            state.check_passed()?;
            store::write_state(&state).await?;
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
        state.check_retries + 1
    } else {
        runtime_context
            .as_ref()
            .map(|context| context.check_retries + 1)
            .unwrap_or(1)
    };
    match check_gate::run(&config, attempt).await? {
        CheckGateResult::Passed => {
            if use_legacy_state {
                state.check_passed()?;
                store::write_state(&state).await?;
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
            info!(state = ?state.state, "All checks passed; staying in current work state");
            Ok(format!(
                "All checks passed. State remains {:?}. Continue working or call /submit when the task is ready for review.",
                state.state
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
            match state.check_failed(failure.failure_reason, config.limits.max_check_retries) {
                Ok(()) => {
                    store::write_state(&state).await?;
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
                    store::write_state(&state).await?;
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

fn should_use_legacy_state(
    state: &crate::state::machine::StateData,
    context: Option<&RuntimeTaskContext>,
) -> bool {
    context.is_none()
        || context.is_some_and(|context| {
            state.active_task_id.as_deref() == Some(context.task_id.as_str())
        })
}
