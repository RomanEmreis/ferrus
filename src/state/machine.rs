use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskState {
    Idle,
    Executing,
    Checking,
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
            claimed_by: None,
            lease_until: None,
            last_heartbeat: None,
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

    #[error("check retry limit reached ({retries} consecutive failures) — state is now Failed; use /reset to recover")]
    CheckLimitExceeded { retries: u32 },

    #[error("review cycle limit reached ({cycles} reject→fix cycles) — state is now Failed; use /reset to recover")]
    ReviewLimitExceeded { cycles: u32 },
}

impl StateData {
    /// Reset to Idle from any state. Used by the HQ `/reset` command.
    pub fn force_reset(&mut self) {
        *self = Self::default();
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
        self.state = TaskState::Executing;
        self.check_retries = 0;
        self.review_cycles = 0;
        self.failure_reason = None;
        Ok(())
    }

    /// `Executing | Addressing → Checking`. Called when `/check` passes.
    ///
    /// Clears consecutive check-failure metadata because the current code now
    /// satisfies the configured checks.
    pub fn check_passed(&mut self) -> Result<(), TransitionError> {
        match self.state {
            TaskState::Executing | TaskState::Addressing => {
                self.state = TaskState::Checking;
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

    /// `Executing | Addressing → Addressing | Failed`. Called when `/check` fails.
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
                    self.state = TaskState::Addressing;
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

    /// `Checking → Reviewing`. Called by Executor via `/submit`.
    pub fn submit(&mut self) -> Result<(), TransitionError> {
        if self.state != TaskState::Checking {
            return Err(TransitionError::InvalidTransition {
                action: "submit",
                state: self.state.clone(),
            });
        }
        self.clear_lease();
        self.state = TaskState::Reviewing;
        Ok(())
    }

    /// `Executing | Addressing | Checking → Consultation`. Called by `/consult`.
    ///
    /// Returns the paused state so the caller can log it.
    pub fn consult(&mut self) -> Result<TaskState, TransitionError> {
        match self.state {
            TaskState::Executing | TaskState::Addressing | TaskState::Checking => {
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
            | TaskState::Checking
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
        *self = Self::default();
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
        assert_eq!(s.state, TaskState::Checking);
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
        assert_eq!(s.state, TaskState::Addressing);
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
    fn check_pass_clears_failure_reason_and_retries() {
        let mut s = idle();
        s.create_task().unwrap();
        s.check_failed("bad".into(), 5).unwrap();

        s.check_passed().unwrap();

        assert_eq!(s.state, TaskState::Checking);
        assert_eq!(s.check_retries, 0);
        assert!(s.failure_reason.is_none());
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
        s.create_task().unwrap();
        for i in 1..=5 {
            let _ = s.check_failed(format!("fail {i}"), 5);
        }
        assert_eq!(s.state, TaskState::Failed);
        s.reset().unwrap();
        assert_eq!(s.state, TaskState::Idle);
        assert_eq!(s.check_retries, 0);
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
        let mut s = state_in(TaskState::Checking);

        let paused = s.consult().unwrap();
        assert_eq!(paused, TaskState::Checking);
        assert_eq!(s.state, TaskState::Consultation);
        assert_eq!(s.paused_state, Some(TaskState::Checking));

        let resumed = s.finish_consult().unwrap();
        assert_eq!(resumed, TaskState::Checking);
        assert_eq!(s.state, TaskState::Checking);
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
            TaskState::Checking,
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
            TaskState::Checking,
            TaskState::Consultation,
            TaskState::Reviewing,
        ] {
            let mut s = state_in(TaskState::AwaitingHuman);
            s.paused_state = Some(paused.clone());

            let resumed = s.answer().unwrap();

            assert_eq!(resumed, paused);
            assert_eq!(s.state, paused);
            assert!(s.paused_state.is_none());
        }
    }

    #[test]
    fn answer_from_non_awaiting_human_state_fails() {
        for state in [
            TaskState::Idle,
            TaskState::Executing,
            TaskState::Checking,
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
