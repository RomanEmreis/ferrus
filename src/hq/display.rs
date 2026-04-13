use tokio::sync::{mpsc, oneshot};

use crate::state::agents::{AgentStatus, AgentsRegistry};

use super::{
    state_watcher::{format_elapsed, TransitionSnapshot, WatchedState},
    tui::{StatusSnapshot, UiMessage},
};

#[derive(Clone)]
pub struct Display(pub mpsc::UnboundedSender<UiMessage>);

impl Display {
    pub fn info(&self, msg: impl Into<String>) {
        let _ = self.0.send(UiMessage::Info(msg.into()));
    }

    pub fn tip(&self, msg: impl Into<String>) {
        let _ = self.0.send(UiMessage::Tip(msg.into()));
    }

    pub fn muted(&self, msg: impl Into<String>) {
        let _ = self.0.send(UiMessage::Muted(msg.into()));
    }

    pub fn error(&self, msg: impl Into<String>) {
        let _ = self.0.send(UiMessage::Error(msg.into()));
    }

    pub fn transition(&self, transition: &TransitionSnapshot) {
        let (from, to) = format_transition_parts(transition);
        let _ = self.0.send(UiMessage::Transition { from, to });
    }

    pub fn status(&self, watched: &WatchedState, agents: &AgentsRegistry) {
        let mut snapshot = StatusSnapshot::from_watched_state(watched);
        snapshot.supervisor_status =
            agent_status_label(agents, crate::agent_id::ROLE_SUPERVISOR).to_string();
        snapshot.executor_status =
            agent_status_label(agents, crate::agent_id::ROLE_EXECUTOR).to_string();
        let _ = self.0.send(UiMessage::StatusUpdate(snapshot));

        let state = &watched.state;
        self.info(format!("state      : {:?}", state.state));
        if let Some(by) = &state.claimed_by {
            self.info(format!("claimed_by : {by}"));
        }
        if state.check_retries > 0 {
            self.info(format!("retries    : {}", state.check_retries));
        }
        if state.review_cycles > 0 {
            self.info(format!("cycles     : {}", state.review_cycles));
        }
        if agents.agents.is_empty() {
            self.info("agents     : none");
        } else {
            for agent in &agents.agents {
                let status = match agent.status {
                    AgentStatus::Idle => "idle",
                    AgentStatus::Running => "running",
                    AgentStatus::Suspended => "suspended",
                };
                let pid = agent
                    .pid
                    .map(|pid| format!(" pid={pid}"))
                    .unwrap_or_default();
                self.info(format!("  [{:<10}] {:<10}{}", agent.role, status, pid));
            }
        }
    }

    pub fn suspend(&self) -> oneshot::Receiver<()> {
        let (ack_tx, ack_rx) = oneshot::channel();
        let _ = self.0.send(UiMessage::Suspend { ack: ack_tx });
        ack_rx
    }

    pub fn resume(&self) {
        let _ = self.0.send(UiMessage::Resume);
    }

    pub fn confirm(&self, prompt: impl Into<String>) -> oneshot::Receiver<bool> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.0.send(UiMessage::ConfirmationRequest {
            prompt: prompt.into(),
            reply: reply_tx,
        });
        reply_rx
    }
}

fn agent_status_label<'a>(agents: &'a AgentsRegistry, role: &str) -> &'a str {
    match agents.by_role(role).map(|entry| &entry.status) {
        Some(AgentStatus::Idle) => "idle",
        Some(AgentStatus::Running) => "running",
        Some(AgentStatus::Suspended) => "suspended",
        None => "none",
    }
}

fn format_transition_parts(transition: &TransitionSnapshot) -> (Option<String>, String) {
    let hide_elapsed = transition.from == crate::state::machine::TaskState::Idle
        || transition.to == crate::state::machine::TaskState::Complete;

    let from = if transition.used_total {
        None
    } else if hide_elapsed {
        Some(format!("{:?}", transition.from))
    } else {
        Some(format!(
            "{:?} ({})",
            transition.from,
            format_elapsed(transition.elapsed)
        ))
    };

    let to = if transition.used_total && !hide_elapsed {
        format!(
            "{:?} ({})",
            transition.to,
            format_elapsed(transition.elapsed)
        )
    } else {
        format!("{:?}", transition.to)
    };

    (from, to)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::state::machine::TaskState;

    use super::{format_transition_parts, TransitionSnapshot};

    #[test]
    fn hides_elapsed_when_transition_starts_from_idle() {
        let transition = TransitionSnapshot {
            from: TaskState::Idle,
            to: TaskState::Executing,
            elapsed: Duration::from_secs(84),
            used_total: false,
        };

        let (from, to) = format_transition_parts(&transition);

        assert_eq!(from, Some("Idle".to_string()));
        assert_eq!(to, "Executing");
    }

    #[test]
    fn hides_elapsed_when_transition_ends_at_complete() {
        let transition = TransitionSnapshot {
            from: TaskState::Reviewing,
            to: TaskState::Complete,
            elapsed: Duration::from_secs(84),
            used_total: true,
        };

        let (from, to) = format_transition_parts(&transition);

        assert_eq!(from, None);
        assert_eq!(to, "Complete");
    }

    #[test]
    fn keeps_elapsed_for_other_transitions() {
        let transition = TransitionSnapshot {
            from: TaskState::Executing,
            to: TaskState::Addressing,
            elapsed: Duration::from_secs(84),
            used_total: false,
        };

        let (from, to) = format_transition_parts(&transition);

        assert_eq!(from, Some("Executing (1m 24s)".to_string()));
        assert_eq!(to, "Addressing");
    }
}
