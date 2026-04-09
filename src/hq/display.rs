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

    pub fn error(&self, msg: impl Into<String>) {
        let _ = self.0.send(UiMessage::Error(msg.into()));
    }

    pub fn transition(&self, transition: &TransitionSnapshot) {
        let _ = self.0.send(UiMessage::Transition {
            from: if transition.used_total {
                None
            } else {
                Some(format!(
                    "{:?} ({})",
                    transition.from,
                    format_elapsed(transition.elapsed)
                ))
            },
            to: if transition.used_total {
                format!(
                    "{:?} ({})",
                    transition.to,
                    format_elapsed(transition.elapsed)
                )
            } else {
                format!("{:?}", transition.to)
            },
        });
    }

    pub fn status(&self, watched: &WatchedState, agents: &AgentsRegistry) {
        let mut snapshot = StatusSnapshot::from_watched_state(watched);
        snapshot.supervisor_status = agent_status_label(agents, "supervisor").to_string();
        snapshot.executor_status = agent_status_label(agents, "executor").to_string();
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
