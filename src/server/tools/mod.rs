pub mod answer;
pub mod approve;
pub mod ask_human;
pub mod check;
pub mod check_gate;
pub mod consult;
pub mod create_spec;
pub mod create_task;
pub mod heartbeat;
pub mod reject;
pub mod reset;
pub mod respond_consult;
pub mod review_pending;
pub mod status;
pub mod submit;
pub mod wait_for_answer;
pub mod wait_for_consult;
pub mod wait_for_review;
pub mod wait_for_task;

use neva::prelude::*;

use crate::{
    agent_id::ROLE_SUPERVISOR,
    state::machine::{StateData, TaskState},
};

/// Convert an [`anyhow::Error`] into a neva tool error.
pub(super) fn tool_err(e: anyhow::Error) -> Error {
    Error::new(
        ErrorCode::InternalError,
        std::io::Error::other(e.to_string()),
    )
}

pub(super) fn ensure_lease_owner(state: &StateData, agent_id: &str) -> anyhow::Result<()> {
    ensure_lease_identity(state, agent_id)?;
    if state.lease_expired() {
        anyhow::bail!(
            "Cannot modify task: lease for {agent_id} has expired. Call wait_for_task again to reclaim work."
        );
    }
    Ok(())
}

pub(super) fn ensure_lease_identity(state: &StateData, agent_id: &str) -> anyhow::Result<()> {
    if state.claimed_by.as_deref() != Some(agent_id) {
        let owner = state.claimed_by.as_deref().unwrap_or("none");
        anyhow::bail!("Cannot modify task: lease is held by {owner}, not {agent_id}");
    }
    Ok(())
}

pub(super) fn ensure_can_ask_human(state: &StateData, agent_id: &str) -> anyhow::Result<()> {
    if state.state == TaskState::Consultation && agent_role(agent_id) == Some(ROLE_SUPERVISOR) {
        return Ok(());
    }
    ensure_lease_owner(state, agent_id)
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
