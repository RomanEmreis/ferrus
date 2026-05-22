pub mod answer;
pub mod approve;
pub mod ask_human;
pub mod check;
pub mod check_gate;
pub mod consult;
pub mod create_spec;
pub mod create_task;
pub mod enqueue_task;
pub mod heartbeat;
pub mod reject;
pub mod reset;
pub mod respond_consult;
pub mod review_pending;
pub mod status;
pub mod submit;
pub mod wait_for_answer;
pub mod wait_for_consult;
pub mod wait_for_consultation;
pub mod wait_for_review;
pub mod wait_for_task;

use neva::prelude::*;

use crate::{
    agent_id::ROLE_SUPERVISOR,
    project::{self, RuntimeTaskContext, TaskClaim},
    state::machine::{StateData, TaskState},
    state::store,
};

/// Convert an [`anyhow::Error`] into a neva tool error.
pub(super) fn tool_err(e: anyhow::Error) -> Error {
    Error::new(
        ErrorCode::InternalError,
        std::io::Error::other(e.to_string()),
    )
}

#[cfg(test)]
pub(super) fn ensure_lease_owner(state: &StateData, agent_id: &str) -> anyhow::Result<()> {
    ensure_lease_identity(state, agent_id)?;
    if state.lease_expired() {
        anyhow::bail!(
            "Cannot modify task: lease for {agent_id} has expired. Call wait_for_task again to reclaim work."
        );
    }
    Ok(())
}

pub(super) async fn ensure_lease_owner_or_reclaim(
    state: &mut StateData,
    agent_id: &str,
    ttl_secs: u64,
) -> anyhow::Result<()> {
    if state.claimed_by.as_deref() == Some(agent_id) {
        if !state.lease_expired() {
            return Ok(());
        }

        return reclaim_state_active_task_lease(state, agent_id, ttl_secs).await;
    }

    if let Some(context) = project::runtime_task_context_for_agent(agent_id).await? {
        match project::claim_task(&context.task_id, &context.task_path, agent_id, ttl_secs).await {
            Ok(TaskClaim::Claimed { .. }) | Ok(TaskClaim::AlreadyClaimed { .. }) => {
                return Ok(());
            }
            Ok(TaskClaim::ClaimedByOther { claimed_by }) => {
                anyhow::bail!("Cannot modify task: lease is held by {claimed_by}, not {agent_id}");
            }
            Err(err) => {
                tracing::warn!(
                    error = ?err,
                    agent_id,
                    task_id = context.task_id,
                    "failed to validate lease in ferrus.db"
                );
            }
        }
    }

    ensure_lease_identity(state, agent_id)?;
    Ok(())
}

pub(super) async fn runtime_task_context_for_agent_best_effort(
    agent_id: &str,
) -> Option<RuntimeTaskContext> {
    match project::runtime_task_context_for_agent(agent_id).await {
        Ok(context) => context,
        Err(err) => {
            tracing::warn!(
                error = ?err,
                agent_id,
                "failed to resolve runtime task context from ferrus.db"
            );
            None
        }
    }
}

async fn reclaim_state_active_task_lease(
    state: &mut StateData,
    agent_id: &str,
    ttl_secs: u64,
) -> anyhow::Result<()> {
    match state.state {
        TaskState::Executing | TaskState::Addressing | TaskState::Consultation => {
            match project::claim_current_task(agent_id, ttl_secs).await {
                Ok(TaskClaim::Claimed {
                    claimed_by,
                    lease_until,
                })
                | Ok(TaskClaim::AlreadyClaimed {
                    claimed_by,
                    lease_until,
                }) => {
                    state.claimed_by = Some(claimed_by);
                    state.lease_until = Some(lease_until);
                    state.last_heartbeat = Some(chrono::Utc::now());
                    store::write_state(state).await?;
                    Ok(())
                }
                Ok(TaskClaim::ClaimedByOther { claimed_by }) => {
                    anyhow::bail!(
                        "Cannot modify task: lease is held by {claimed_by}, not {agent_id}"
                    );
                }
                Err(err) => {
                    tracing::warn!(
                        error = ?err,
                        agent_id,
                        "failed to reclaim lease in ferrus.db; falling back to STATE.json lease"
                    );
                    store::claim_state(agent_id, ttl_secs, state).await?;
                    Ok(())
                }
            }
        }
        TaskState::Reviewing => {
            store::claim_state(agent_id, ttl_secs, state).await?;
            Ok(())
        }
        _ => anyhow::bail!(
            "Cannot modify task: lease for {agent_id} has expired. Call wait_for_task again to reclaim work."
        ),
    }
}

pub(super) fn ensure_lease_identity(state: &StateData, agent_id: &str) -> anyhow::Result<()> {
    if state.claimed_by.as_deref() != Some(agent_id) {
        let owner = state.claimed_by.as_deref().unwrap_or("none");
        anyhow::bail!("Cannot modify task: lease is held by {owner}, not {agent_id}");
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn ensure_can_ask_human(state: &StateData, agent_id: &str) -> anyhow::Result<()> {
    if state.state == TaskState::Consultation && agent_role(agent_id) == Some(ROLE_SUPERVISOR) {
        return Ok(());
    }
    ensure_lease_owner(state, agent_id)
}

pub(super) async fn ensure_can_ask_human_or_reclaim(
    state: &mut StateData,
    agent_id: &str,
    ttl_secs: u64,
) -> anyhow::Result<()> {
    if state.state == TaskState::Consultation && agent_role(agent_id) == Some(ROLE_SUPERVISOR) {
        return Ok(());
    }
    ensure_lease_owner_or_reclaim(state, agent_id, ttl_secs).await
}

pub(super) fn ensure_answer_waiter(state: &StateData, agent_id: &str) -> anyhow::Result<()> {
    if let Some(waiter) = state.awaiting_human_by.as_deref() {
        if waiter == agent_id {
            return Ok(());
        }
        anyhow::bail!("Cannot wait for answer: question was asked by {waiter}, not {agent_id}");
    }

    if state.paused_state == Some(TaskState::Consultation)
        && agent_role(agent_id) == Some(ROLE_SUPERVISOR)
    {
        return Ok(());
    }

    ensure_lease_identity(state, agent_id)
}

fn agent_role(agent_id: &str) -> Option<&str> {
    agent_id.split_once(':').map(|(role, _)| role)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use tempfile::TempDir;

    async fn setup_runtime_project() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ferrus")).unwrap();
        let data_dir = dir.path().join(".ferrus/projects/test-project");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
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

    #[test]
    fn lease_owner_check_accepts_current_unexpired_owner() {
        let mut state = StateData::default();
        state.claimed_by = Some("executor:codex:1".to_string());
        state.lease_until = Some(Utc::now() + chrono::Duration::seconds(60));

        ensure_lease_owner(&state, "executor:codex:1").unwrap();
    }

    #[test]
    fn lease_owner_check_rejects_other_agent_and_expired_lease() {
        let mut state = StateData::default();
        state.claimed_by = Some("executor:codex:1".to_string());
        state.lease_until = Some(Utc::now() + chrono::Duration::seconds(60));

        let err = ensure_lease_owner(&state, "executor:codex:2")
            .unwrap_err()
            .to_string();
        assert!(err.contains("lease is held by executor:codex:1"));

        state.lease_until = Some(Utc::now() - chrono::Duration::seconds(1));
        let err = ensure_lease_owner(&state, "executor:codex:1")
            .unwrap_err()
            .to_string();
        assert!(err.contains("has expired"));
        ensure_lease_identity(&state, "executor:codex:1").unwrap();
    }

    #[tokio::test]
    async fn lease_owner_check_accepts_agent_database_task_context() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_runtime_project().await;
        crate::project::record_task_status("t-002", ".ferrus/tasks/t-002.md", "executing")
            .await
            .unwrap();
        crate::project::claim_task("t-002", ".ferrus/tasks/t-002.md", "executor:codex:2", 60)
            .await
            .unwrap();
        let mut state = StateData {
            state: TaskState::Executing,
            claimed_by: Some("executor:codex:1".to_string()),
            lease_until: Some(Utc::now() + chrono::Duration::seconds(60)),
            ..StateData::default()
        };

        ensure_lease_owner_or_reclaim(&mut state, "executor:codex:2", 60)
            .await
            .unwrap();

        teardown(previous);
    }

    #[test]
    fn ask_human_allows_supervisor_during_consultation_only() {
        let mut state = StateData {
            state: TaskState::Consultation,
            claimed_by: Some("executor:codex:1".to_string()),
            lease_until: Some(Utc::now() + chrono::Duration::seconds(60)),
            ..StateData::default()
        };

        ensure_can_ask_human(&state, "supervisor:claude-code:1").unwrap();

        state.state = TaskState::Executing;
        let err = ensure_can_ask_human(&state, "supervisor:claude-code:1")
            .unwrap_err()
            .to_string();
        assert!(err.contains("lease is held by executor:codex:1"));
    }

    #[test]
    fn answer_waiter_check_uses_recorded_question_owner() {
        let state = StateData {
            state: TaskState::AwaitingHuman,
            awaiting_human_by: Some("supervisor:claude-code:1".to_string()),
            claimed_by: Some("executor:codex:1".to_string()),
            ..StateData::default()
        };

        ensure_answer_waiter(&state, "supervisor:claude-code:1").unwrap();

        let err = ensure_answer_waiter(&state, "executor:codex:1")
            .unwrap_err()
            .to_string();
        assert!(err.contains("question was asked by supervisor:claude-code:1"));
    }

    #[test]
    fn answer_waiter_check_has_legacy_fallbacks() {
        let mut state = StateData {
            state: TaskState::AwaitingHuman,
            paused_state: Some(TaskState::Consultation),
            claimed_by: Some("executor:codex:1".to_string()),
            ..StateData::default()
        };

        ensure_answer_waiter(&state, "supervisor:claude-code:1").unwrap();

        state.paused_state = Some(TaskState::Executing);
        ensure_answer_waiter(&state, "executor:codex:1").unwrap();
    }
}
