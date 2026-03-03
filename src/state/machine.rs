use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskState {
    Idle,
    Executing,
    Checking,
    Reviewing,
    Addressing,
    Complete,
    Failed,
    /// Waiting for a human to answer a question (elicitation fallback path).
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
}

const fn default_schema_version() -> u32 { 1 }
const fn default_updated_at() -> DateTime<Utc> { DateTime::UNIX_EPOCH }

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
        }
    }
}

#[derive(Debug, Error)]
pub enum TransitionError {
    #[error("cannot {action} from state {state:?} — current state is invalid for this operation")]
    InvalidTransition { action: &'static str, state: TaskState },

    #[error("check retry limit reached ({retries} consecutive failures) — state is now Failed; use /reset to recover")]
    CheckLimitExceeded { retries: u32 },

    #[error("review cycle limit reached ({cycles} reject→fix cycles) — state is now Failed; use /reset to recover")]
    ReviewLimitExceeded { cycles: u32 },
}

impl StateData {
    /// `Idle → Executing`. Called by Supervisor via `/create_task`.
    pub fn create_task(&mut self) -> Result<(), TransitionError> {
        if self.state != TaskState::Idle {
            return Err(TransitionError::InvalidTransition {
                action: "create_task",
                state: self.state.clone(),
            });
        }
        self.state = TaskState::Executing;
        self.check_retries = 0;
        self.review_cycles = 0;
        self.failure_reason = None;
        Ok(())
    }

    /// `Executing | Addressing → Checking`. Called when `/check` passes.
    pub fn check_passed(&mut self) -> Result<(), TransitionError> {
        match self.state {
            TaskState::Executing | TaskState::Addressing => {
                self.state = TaskState::Checking;
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
        self.state = TaskState::Reviewing;
        Ok(())
    }

    /// `Reviewing → Complete`. Called by Supervisor via `/approve`.
    pub fn approve(&mut self) -> Result<(), TransitionError> {
        if self.state != TaskState::Reviewing {
            return Err(TransitionError::InvalidTransition {
                action: "approve",
                state: self.state.clone(),
            });
        }
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
            self.state = TaskState::Failed;
            self.failure_reason = Some(format!(
                "Task rejected {max_cycles} times without resolution."
            ));
            Err(TransitionError::ReviewLimitExceeded {
                cycles: self.review_cycles,
            })
        } else {
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
}
