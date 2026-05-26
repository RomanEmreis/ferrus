use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskState {
    Idle,
    Executing,
    Consultation,
    Reviewing,
    Addressing,
    Complete,
    Failed,
    /// Waiting for a human to answer a question written to QUESTION.md.
    /// The previous state is saved in `StateData::paused_state` and restored by `/answer`.
    AwaitingHuman,
}

/// Persisted to `.ferrus/STATE.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateData {
    /// Incremented on breaking schema changes so readers can detect stale files.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub state: TaskState,
    pub check_retries: u32,
    pub review_cycles: u32,
    pub failure_reason: Option<String>,
    /// RFC 3339 timestamp of the last write. Stamped by `store::write_state`.
    /// Defaults to the Unix epoch when deserializing pre-versioned files.
    #[serde(default = "default_updated_at")]
    pub updated_at: DateTime<Utc>,
    /// PID of the process that last wrote this file. Stamped by `store::write_state`.
    #[serde(default)]
    pub owner_pid: u32,
    /// State to restore when `/answer` is called after an `/ask_human` fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_state: Option<TaskState>,
    /// Agent that asked the pending human question and is allowed to consume the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub awaiting_human_by: Option<String>,
    /// Agent that currently holds the task lease, e.g. "executor:codex:1".
    /// None when the task is unclaimed or in a terminal state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_by: Option<String>,
    /// Timestamp after which the lease is considered expired.
    /// None when unclaimed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_until: Option<DateTime<Utc>>,
    /// Timestamp of the last /heartbeat call.
    /// None when unclaimed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_heartbeat: Option<DateTime<Utc>>,
    /// Relative path or configured path of the currently selected spec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_spec: Option<String>,
    /// Stable milestone ID selected inside `selected_spec`, e.g. "m1.1".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_milestone: Option<String>,
    /// Spec selected as the origin for the next task being drafted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_task_spec: Option<String>,
    /// Milestone selected as the origin for the next task being drafted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_task_milestone: Option<String>,
    /// Spec that originated the currently active task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_spec: Option<String>,
    /// Milestone that originated the currently active task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_milestone: Option<String>,
    /// Stable task artifact id for the active task, e.g. "t-001".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_task_id: Option<String>,
    /// Markdown path for the active task description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_task_path: Option<String>,
    /// Directory containing active run artifacts such as REVIEW.md and SUBMISSION.md.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_run_dir: Option<String>,
}

const fn default_schema_version() -> u32 {
    1
}
const fn default_updated_at() -> DateTime<Utc> {
    DateTime::UNIX_EPOCH
}

impl Default for StateData {
    fn default() -> Self {
        Self {
            schema_version: 1,
            state: TaskState::Idle,
            check_retries: 0,
            review_cycles: 0,
            failure_reason: None,
            updated_at: Utc::now(),
            owner_pid: std::process::id(),
            paused_state: None,
            awaiting_human_by: None,
            claimed_by: None,
            lease_until: None,
            last_heartbeat: None,
            selected_spec: None,
            selected_milestone: None,
            pending_task_spec: None,
            pending_task_milestone: None,
            task_spec: None,
            task_milestone: None,
            active_task_id: None,
            active_task_path: None,
            active_run_dir: None,
        }
    }
}

#[derive(Debug, Error)]
pub enum TransitionError {
    #[error("cannot {action} from state {state:?} — current state is invalid for this operation")]
    InvalidTransition {
        action: &'static str,
        state: TaskState,
    },

    #[error(
        "check retry limit reached ({retries} consecutive failures) — state is now Failed; use /reset to recover"
    )]
    CheckLimitExceeded { retries: u32 },

    #[error(
        "review cycle limit reached ({cycles} reject→fix cycles) — state is now Failed; use /reset to recover"
    )]
    ReviewLimitExceeded { cycles: u32 },
}

impl StateData {
    /// Reset to Idle from any state. Used by the HQ `/reset` command.
    pub fn force_reset(&mut self) {
        let selected_spec = self.selected_spec.clone();
        let selected_milestone = self.selected_milestone.clone();
        *self = Self::default();
        self.selected_spec = selected_spec;
        self.selected_milestone = selected_milestone;
    }

    pub fn set_pending_task_origin(&mut self, spec: Option<String>, milestone: Option<String>) {
        self.pending_task_spec = spec;
        self.pending_task_milestone = milestone;
    }

    #[allow(dead_code)]
    pub fn clear_selected_spec_and_milestone(&mut self) {
        self.selected_spec = None;
        self.selected_milestone = None;
    }

    pub fn set_active_task_artifacts(
        &mut self,
        task_id: String,
        task_path: String,
        run_dir: String,
    ) {
        self.active_task_id = Some(task_id);
        self.active_task_path = Some(task_path);
        self.active_run_dir = Some(run_dir);
    }

    /// True if a non-expired lease exists (`lease_until` is set and in the future).
    #[allow(dead_code)]
    pub fn is_claimed(&self) -> bool {
        self.lease_until.is_some_and(|t| Utc::now() < t)
    }

    /// True if this specific agent holds a valid (non-expired) lease.
    #[allow(dead_code)]
    pub fn is_claimed_by(&self, agent_id: &str) -> bool {
        self.claimed_by.as_deref() == Some(agent_id) && self.is_claimed()
    }

    /// True when `lease_until` is `None` or has been reached/passed.
    /// Returns `true` for unclaimed state so `!state.is_claimed()` is the correct
    /// claim check in `wait_for_task`.
    #[allow(dead_code)]
    pub fn lease_expired(&self) -> bool {
        self.lease_until.is_none_or(|t| Utc::now() >= t)
    }

    /// Clear all lease fields. Called by transition methods that hand off ownership
    /// between roles or to a terminal state.
    #[allow(dead_code)]
    pub fn clear_lease(&mut self) {
        self.claimed_by = None;
        self.lease_until = None;
        self.last_heartbeat = None;
    }

    /// `Idle → Executing`. Called by Supervisor via `/create_task`.
    pub fn create_task(&mut self) -> Result<(), TransitionError> {
        if self.state != TaskState::Idle {
            return Err(TransitionError::InvalidTransition {
                action: "create_task",
                state: self.state.clone(),
            });
        }
        self.clear_lease();
        self.task_spec = self.pending_task_spec.take();
        self.task_milestone = self.pending_task_milestone.take();
        self.state = TaskState::Executing;
        self.check_retries = 0;
        self.review_cycles = 0;
        self.failure_reason = None;
        Ok(())
    }

    /// `/check` passed while the task remains in its current work state.
    ///
    /// Clears consecutive check-failure metadata because the current code now
    /// satisfies the configured checks. Does not move the task into a separate
    /// state; a green check is diagnostic until `/submit` performs the final gate.
    pub fn check_passed(&mut self) -> Result<(), TransitionError> {
        match self.state {
            TaskState::Executing | TaskState::Addressing => {
                self.check_retries = 0;
                self.failure_reason = None;
                Ok(())
            }
            _ => Err(TransitionError::InvalidTransition {
                action: "check (pass)",
                state: self.state.clone(),
            }),
        }
    }

    /// `Executing | Addressing → same state | Failed`. Called when `/check` fails.
    ///
    /// Returns `Err(CheckLimitExceeded)` when the limit is hit (state is already set to `Failed`).
    pub fn check_failed(
        &mut self,
        reason: String,
        max_retries: u32,
    ) -> Result<(), TransitionError> {
        match self.state {
            TaskState::Executing | TaskState::Addressing => {
                self.check_retries += 1;
                if self.check_retries >= max_retries {
                    self.state = TaskState::Failed;
                    self.failure_reason = Some(format!(
                        "Check failed {max_retries} consecutive times. Last failure:\n{reason}"
                    ));
                    Err(TransitionError::CheckLimitExceeded {
                        retries: self.check_retries,
                    })
                } else {
                    self.failure_reason = Some(reason);
                    Ok(())
                }
            }
            _ => Err(TransitionError::InvalidTransition {
                action: "check (fail)",
                state: self.state.clone(),
            }),
        }
    }

    /// `Executing | Addressing → Reviewing`. Called by Executor via `/submit`
    /// after the tool has run a final successful check gate.
    pub fn submit(&mut self) -> Result<(), TransitionError> {
        match self.state {
            TaskState::Executing | TaskState::Addressing => {
                self.clear_lease();
                self.state = TaskState::Reviewing;
                Ok(())
            }
            _ => Err(TransitionError::InvalidTransition {
                action: "submit",
                state: self.state.clone(),
            }),
        }
    }

    /// `Executing | Addressing → Consultation`. Called by `/consult`.
    ///
    /// Returns the paused state so the caller can log it.
    pub fn consult(&mut self) -> Result<TaskState, TransitionError> {
        match self.state {
            TaskState::Executing | TaskState::Addressing => {
                let paused = self.state.clone();
                self.paused_state = Some(paused.clone());
                self.state = TaskState::Consultation;
                Ok(paused)
            }
            _ => Err(TransitionError::InvalidTransition {
                action: "consult",
                state: self.state.clone(),
            }),
        }
    }

    /// `Consultation → paused_state`. Called by `/wait_for_consult`.
    ///
    /// Returns the restored state so the caller can log it.
    pub fn finish_consult(&mut self) -> Result<TaskState, TransitionError> {
        if self.state != TaskState::Consultation {
            return Err(TransitionError::InvalidTransition {
                action: "wait_for_consult",
                state: self.state.clone(),
            });
        }
        let resumed = self.paused_state.take().unwrap_or(TaskState::Idle);
        self.state = resumed.clone();
        Ok(resumed)
    }

    /// `Reviewing → Complete`. Called by Supervisor via `/approve`.
    pub fn approve(&mut self) -> Result<(), TransitionError> {
        if self.state != TaskState::Reviewing {
            return Err(TransitionError::InvalidTransition {
                action: "approve",
                state: self.state.clone(),
            });
        }
        self.clear_lease();
        self.state = TaskState::Complete;
        Ok(())
    }

    /// `Reviewing → Addressing | Failed`. Called by Supervisor via `/reject`.
    ///
    /// Resets `check_retries` so the executor gets a fresh retry budget.
    /// Returns `Err(ReviewLimitExceeded)` when the cycle limit is hit.
    pub fn reject(&mut self, max_cycles: u32) -> Result<(), TransitionError> {
        if self.state != TaskState::Reviewing {
            return Err(TransitionError::InvalidTransition {
                action: "reject",
                state: self.state.clone(),
            });
        }
        self.review_cycles += 1;
        if self.review_cycles >= max_cycles {
            self.clear_lease();
            self.state = TaskState::Failed;
            self.failure_reason = Some(format!(
                "Task rejected {max_cycles} times without resolution."
            ));
            Err(TransitionError::ReviewLimitExceeded {
                cycles: self.review_cycles,
            })
        } else {
            self.clear_lease();
            self.state = TaskState::Addressing;
            self.check_retries = 0;
            Ok(())
        }
    }

    /// Pause any active state into `AwaitingHuman`. Called by `/ask_human` fallback.
    ///
    /// Returns the paused state so the caller can log it.
    pub fn ask_human(&mut self) -> Result<TaskState, TransitionError> {
        match self.state {
            TaskState::Executing
            | TaskState::Addressing
            | TaskState::Consultation
            | TaskState::Reviewing => {
                let paused = self.state.clone();
                self.paused_state = Some(paused.clone());
                self.state = TaskState::AwaitingHuman;
                Ok(paused)
            }
            _ => Err(TransitionError::InvalidTransition {
                action: "ask_human",
                state: self.state.clone(),
            }),
        }
    }

    /// Resume from `AwaitingHuman` back to the paused state. Called by `/answer`.
    ///
    /// Returns the restored state so the caller can log it.
    pub fn answer(&mut self) -> Result<TaskState, TransitionError> {
        if self.state != TaskState::AwaitingHuman {
            return Err(TransitionError::InvalidTransition {
                action: "answer",
                state: self.state.clone(),
            });
        }
        let resumed = self.paused_state.take().unwrap_or(TaskState::Idle);
        self.awaiting_human_by = None;
        self.state = resumed.clone();
        Ok(resumed)
    }

    /// `Failed → Idle`. Human-facing escape hatch via `/reset`.
    pub fn reset(&mut self) -> Result<(), TransitionError> {
        if self.state != TaskState::Failed {
            return Err(TransitionError::InvalidTransition {
                action: "reset",
                state: self.state.clone(),
            });
        }
        self.force_reset();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle() -> StateData {
        StateData::default()
    }

    fn state_in(state: TaskState) -> StateData {
        StateData {
            state,
            ..StateData::default()
        }
    }

    #[test]
    fn create_task_from_idle() {
        let mut s = idle();
        s.create_task().unwrap();
        assert_eq!(s.state, TaskState::Executing);
    }

    #[test]
    fn create_task_promotes_pending_task_origin() {
        let mut s = idle();
        s.set_pending_task_origin(
            Some("docs/specs/spec.md".to_string()),
            Some("m1.0".to_string()),
        );

        s.create_task().unwrap();

        assert_eq!(s.task_spec.as_deref(), Some("docs/specs/spec.md"));
        assert_eq!(s.task_milestone.as_deref(), Some("m1.0"));
        assert!(s.pending_task_spec.is_none());
        assert!(s.pending_task_milestone.is_none());
    }

    #[test]
    fn create_task_from_non_idle_fails() {
        let mut s = idle();
        s.create_task().unwrap();
        assert!(s.create_task().is_err());
    }

    #[test]
    fn check_pass_then_submit_then_approve() {
        let mut s = idle();
        s.create_task().unwrap();
        s.check_passed().unwrap();
        assert_eq!(s.state, TaskState::Executing);
        s.submit().unwrap();
        assert_eq!(s.state, TaskState::Reviewing);
        s.approve().unwrap();
        assert_eq!(s.state, TaskState::Complete);
    }

    #[test]
    fn check_fail_increments_retries() {
        let mut s = idle();
        s.create_task().unwrap();
        s.check_failed("oops".into(), 5).unwrap();
        assert_eq!(s.check_retries, 1);
        assert_eq!(s.state, TaskState::Executing);
    }

    #[test]
    fn check_fail_at_limit_transitions_to_failed() {
        let mut s = idle();
        s.create_task().unwrap();
        for i in 1..=4 {
            s.check_failed(format!("fail {i}"), 5).unwrap();
        }
        let err = s.check_failed("final".into(), 5).unwrap_err();
        assert!(matches!(err, TransitionError::CheckLimitExceeded { .. }));
        assert_eq!(s.state, TaskState::Failed);
    }

    #[test]
    fn reject_resets_check_retries() {
        let mut s = idle();
        s.create_task().unwrap();
        s.check_failed("bad".into(), 5).unwrap();
        s.check_passed().unwrap();
        s.submit().unwrap();
        s.reject(3).unwrap();
        assert_eq!(s.check_retries, 0);
        assert_eq!(s.state, TaskState::Addressing);
    }

    #[test]
    fn check_from_addressing_failure_stays_in_addressing() {
        let mut s = state_in(TaskState::Addressing);

        s.check_failed("still broken".into(), 5).unwrap();

        assert_eq!(s.state, TaskState::Addressing);
        assert_eq!(s.check_retries, 1);
    }

    #[test]
    fn check_pass_clears_failure_reason_and_retries() {
        let mut s = idle();
        s.create_task().unwrap();
        s.check_failed("bad".into(), 5).unwrap();

        s.check_passed().unwrap();

        assert_eq!(s.state, TaskState::Executing);
        assert_eq!(s.check_retries, 0);
        assert!(s.failure_reason.is_none());
    }

    #[test]
    fn check_pass_in_addressing_keeps_state_until_submit() {
        let mut s = state_in(TaskState::Addressing);

        s.check_passed().unwrap();
        assert_eq!(s.state, TaskState::Addressing);

        s.submit().unwrap();
        assert_eq!(s.state, TaskState::Reviewing);
    }

    #[test]
    fn reject_at_limit_transitions_to_failed() {
        let mut s = idle();
        s.create_task().unwrap();
        for _ in 0..2 {
            s.check_passed().unwrap();
            s.submit().unwrap();
            s.reject(3).unwrap();
        }
        s.check_passed().unwrap();
        s.submit().unwrap();
        let err = s.reject(3).unwrap_err();
        assert!(matches!(err, TransitionError::ReviewLimitExceeded { .. }));
        assert_eq!(s.state, TaskState::Failed);
    }

    #[test]
    fn reset_from_failed() {
        let mut s = idle();
        s.selected_spec = Some("docs/specs/spec.md".to_string());
        s.selected_milestone = Some("m1.0".to_string());
        s.create_task().unwrap();
        for i in 1..=5 {
            let _ = s.check_failed(format!("fail {i}"), 5);
        }
        assert_eq!(s.state, TaskState::Failed);
        s.reset().unwrap();
        assert_eq!(s.state, TaskState::Idle);
        assert_eq!(s.check_retries, 0);
        assert_eq!(s.selected_spec.as_deref(), Some("docs/specs/spec.md"));
        assert_eq!(s.selected_milestone.as_deref(), Some("m1.0"));
    }

    #[test]
    fn clear_selected_spec_and_milestone_only_clears_selection() {
        let mut s = StateData {
            selected_spec: Some("docs/specs/spec.md".to_string()),
            selected_milestone: Some("m1.0".to_string()),
            pending_task_spec: Some("docs/specs/pending.md".to_string()),
            pending_task_milestone: Some("m2.0".to_string()),
            task_spec: Some("docs/specs/task.md".to_string()),
            task_milestone: Some("m3.0".to_string()),
            ..StateData::default()
        };

        s.clear_selected_spec_and_milestone();

        assert!(s.selected_spec.is_none());
        assert!(s.selected_milestone.is_none());
        assert_eq!(
            s.pending_task_spec.as_deref(),
            Some("docs/specs/pending.md")
        );
        assert_eq!(s.pending_task_milestone.as_deref(), Some("m2.0"));
        assert_eq!(s.task_spec.as_deref(), Some("docs/specs/task.md"));
        assert_eq!(s.task_milestone.as_deref(), Some("m3.0"));
    }

    #[test]
    fn lease_helpers_unclaimed() {
        let s = StateData::default();
        assert!(!s.is_claimed());
        assert!(!s.is_claimed_by("executor:codex:1"));
        assert!(s.lease_expired()); // None counts as expired
    }

    #[test]
    fn consult_round_trip_restores_previous_state() {
        let mut s = state_in(TaskState::Addressing);

        let paused = s.consult().unwrap();
        assert_eq!(paused, TaskState::Addressing);
        assert_eq!(s.state, TaskState::Consultation);
        assert_eq!(s.paused_state, Some(TaskState::Addressing));

        let resumed = s.finish_consult().unwrap();
        assert_eq!(resumed, TaskState::Addressing);
        assert_eq!(s.state, TaskState::Addressing);
        assert!(s.paused_state.is_none());
    }

    #[test]
    fn lease_helpers_claimed() {
        use chrono::Duration;
        let mut s = StateData::default();
        s.claimed_by = Some("executor:codex:1".to_string());
        s.lease_until = Some(Utc::now() + Duration::seconds(60));
        s.last_heartbeat = Some(Utc::now());

        assert!(s.is_claimed());
        assert!(s.is_claimed_by("executor:codex:1"));
        assert!(!s.is_claimed_by("executor:codex:2"));
        assert!(!s.lease_expired());
    }

    #[test]
    fn lease_helpers_expired() {
        use chrono::Duration;
        let mut s = StateData::default();
        s.claimed_by = Some("executor:codex:1".to_string());
        s.lease_until = Some(Utc::now() - Duration::seconds(1)); // in the past
        s.last_heartbeat = Some(Utc::now() - Duration::seconds(31));

        assert!(!s.is_claimed());
        assert!(!s.is_claimed_by("executor:codex:1")); // expired = not claimed
        assert!(s.lease_expired());
    }

    #[test]
    fn create_task_clears_lease() {
        use chrono::Duration;
        let mut s = idle();
        s.claimed_by = Some("supervisor:claude-code:1".to_string());
        s.lease_until = Some(Utc::now() + Duration::seconds(60));
        s.last_heartbeat = Some(Utc::now());
        s.create_task().unwrap();
        assert!(s.claimed_by.is_none());
        assert!(s.lease_until.is_none());
        assert!(s.last_heartbeat.is_none());
    }

    #[test]
    fn submit_clears_lease() {
        use chrono::Duration;
        let mut s = idle();
        s.create_task().unwrap();
        s.check_passed().unwrap();
        s.claimed_by = Some("executor:codex:1".to_string());
        s.lease_until = Some(Utc::now() + Duration::seconds(60));
        s.last_heartbeat = Some(Utc::now());
        s.submit().unwrap();
        assert!(s.claimed_by.is_none());
        assert!(s.lease_until.is_none());
        assert!(s.last_heartbeat.is_none());
    }

    #[test]
    fn approve_clears_lease() {
        use chrono::Duration;
        let mut s = idle();
        s.create_task().unwrap();
        s.check_passed().unwrap();
        s.submit().unwrap();
        s.claimed_by = Some("supervisor:claude-code:1".to_string());
        s.lease_until = Some(Utc::now() + Duration::seconds(60));
        s.last_heartbeat = Some(Utc::now());
        s.approve().unwrap();
        assert!(s.claimed_by.is_none());
        assert!(s.lease_until.is_none());
        assert!(s.last_heartbeat.is_none());
    }

    #[test]
    fn reject_clears_lease() {
        use chrono::Duration;
        let mut s = idle();
        s.create_task().unwrap();
        s.check_passed().unwrap();
        s.submit().unwrap();
        s.claimed_by = Some("supervisor:claude-code:1".to_string());
        s.lease_until = Some(Utc::now() + Duration::seconds(60));
        s.last_heartbeat = Some(Utc::now());
        s.reject(3).unwrap();
        assert!(s.claimed_by.is_none());
        assert!(s.lease_until.is_none());
        assert!(s.last_heartbeat.is_none());
    }

    #[test]
    fn reject_at_limit_clears_lease() {
        use chrono::Duration;
        let mut s = idle();
        s.create_task().unwrap();
        // Drive to the limit via two preceding reject cycles.
        for _ in 0..2 {
            s.check_passed().unwrap();
            s.submit().unwrap();
            s.reject(3).unwrap();
        }
        s.check_passed().unwrap();
        s.submit().unwrap();
        s.claimed_by = Some("supervisor:claude-code:1".to_string());
        s.lease_until = Some(Utc::now() + Duration::seconds(60));
        s.last_heartbeat = Some(Utc::now());
        // This call hits the limit-exceeded (Err) branch.
        let _ = s.reject(3);
        assert!(s.claimed_by.is_none());
        assert!(s.lease_until.is_none());
        assert!(s.last_heartbeat.is_none());
    }

    #[test]
    fn ask_human_from_valid_states_transitions_to_awaiting_human() {
        for state in [
            TaskState::Executing,
            TaskState::Addressing,
            TaskState::Consultation,
            TaskState::Reviewing,
        ] {
            let mut s = state_in(state.clone());
            let paused = s.ask_human().unwrap();

            assert_eq!(paused, state);
            assert_eq!(s.state, TaskState::AwaitingHuman);
            assert_eq!(s.paused_state, Some(paused));
        }
    }

    #[test]
    fn ask_human_from_invalid_states_fails() {
        for state in [
            TaskState::Idle,
            TaskState::Complete,
            TaskState::Failed,
            TaskState::AwaitingHuman,
        ] {
            let mut s = state_in(state.clone());
            let err = s.ask_human().unwrap_err();

            assert!(matches!(
                err,
                TransitionError::InvalidTransition {
                    action: "ask_human",
                    state: err_state,
                } if err_state == state
            ));
        }
    }

    #[test]
    fn answer_restores_paused_state_and_clears_paused_state() {
        for paused in [
            TaskState::Executing,
            TaskState::Addressing,
            TaskState::Consultation,
            TaskState::Reviewing,
        ] {
            let mut s = state_in(TaskState::AwaitingHuman);
            s.paused_state = Some(paused.clone());
            s.awaiting_human_by = Some("executor:codex:1".to_string());

            let resumed = s.answer().unwrap();

            assert_eq!(resumed, paused);
            assert_eq!(s.state, paused);
            assert!(s.paused_state.is_none());
            assert!(s.awaiting_human_by.is_none());
        }
    }

    #[test]
    fn answer_from_non_awaiting_human_state_fails() {
        for state in [
            TaskState::Idle,
            TaskState::Executing,
            TaskState::Reviewing,
            TaskState::Addressing,
            TaskState::Complete,
            TaskState::Failed,
        ] {
            let mut s = state_in(state.clone());
            let err = s.answer().unwrap_err();

            assert!(matches!(
                err,
                TransitionError::InvalidTransition {
                    action: "answer",
                    state: err_state,
                } if err_state == state
            ));
        }
    }

    #[test]
    fn answer_without_paused_state_falls_back_to_idle() {
        let mut s = state_in(TaskState::AwaitingHuman);

        let resumed = s.answer().unwrap();

        assert_eq!(resumed, TaskState::Idle);
        assert_eq!(s.state, TaskState::Idle);
        assert!(s.paused_state.is_none());
    }

    #[test]
    fn create_task_clears_failure_reason_after_reset() {
        let mut s = state_in(TaskState::Failed);
        s.failure_reason = Some("previous failure".into());

        s.reset().unwrap();
        s.failure_reason = Some("stale failure".into());
        s.create_task().unwrap();

        assert_eq!(s.state, TaskState::Executing);
        assert!(s.failure_reason.is_none());
    }
}
