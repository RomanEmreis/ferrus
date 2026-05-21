use tokio::sync::{mpsc, oneshot};

use crate::state::agents::{AgentStatus, AgentsRegistry};

use super::{
    state_watcher::{TransitionSnapshot, WatchedState, format_elapsed},
    tui::{StatusSnapshot, UiMessage},
};

#[derive(Clone)]
pub struct Display(pub mpsc::UnboundedSender<UiMessage>);

impl Display {
    pub fn info(&self, msg: impl Into<String>) {
        let _ = self.0.send(UiMessage::Info(msg.into()));
    }

    pub fn info_block(&self, lines: impl IntoIterator<Item = String>) {
        let text = lines.into_iter().collect::<Vec<_>>().join("\n");
        if !text.is_empty() {
            self.info(text);
        }
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
        let mut lines = vec![format!("state      : {:?}", state.state)];
        if let Some(by) = &state.claimed_by {
            lines.push(format!("claimed_by : {by}"));
        }
        if state.check_retries > 0 {
            lines.push(format!("retries    : {}", state.check_retries));
        }
        if state.review_cycles > 0 {
            lines.push(format!("cycles     : {}", state.review_cycles));
        }
        if let Some(spec) = &state.selected_spec {
            lines.push(format!("spec       : {spec}"));
        }
        if let Some(milestone) = &state.selected_milestone {
            lines.push(format!("milestone  : {milestone}"));
        }
        if agents.agents.is_empty() {
            lines.push("agents     : none".to_string());
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
                lines.push(format!("  [{:<10}] {:<10}{}", agent.role, status, pid));
            }
        }
        self.info_block(lines);
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
        self.confirm_custom(prompt, "[y/N]", false, &['y'], &['n'])
    }

    pub fn confirm_yes(&self, prompt: impl Into<String>) -> oneshot::Receiver<bool> {
        self.confirm_custom(prompt, "[Y/n]", true, &['y'], &['n'])
    }

    pub fn confirm_continue(&self, prompt: impl Into<String>) -> oneshot::Receiver<bool> {
        self.confirm_custom(prompt, "[c/N]", false, &['c'], &['n'])
    }

    fn confirm_custom(
        &self,
        prompt: impl Into<String>,
        suffix: impl Into<String>,
        default: bool,
        accept_keys: &[char],
        reject_keys: &[char],
    ) -> oneshot::Receiver<bool> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.0.send(UiMessage::ConfirmationRequest {
            prompt: prompt.into(),
            suffix: suffix.into(),
            default,
            accept_keys: accept_keys.to_vec(),
            reject_keys: reject_keys.to_vec(),
            reply: reply_tx,
        });
        reply_rx
    }

    pub fn select(
        &self,
        prompt: impl Into<String>,
        options: Vec<String>,
    ) -> oneshot::Receiver<Option<usize>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.0.send(UiMessage::SelectionRequest {
            prompt: prompt.into(),
            options,
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

    use tokio::sync::mpsc;

    use crate::{
        agent_id::ROLE_SUPERVISOR,
        hq::{state_watcher::WatchedState, tui::UiMessage},
        state::{
            agents::{AgentEntry, AgentStatus, AgentsRegistry},
            machine::{StateData, TaskState},
        },
    };

    use super::{Display, TransitionSnapshot, format_transition_parts};

    #[test]
    fn info_block_sends_one_multiline_message() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let display = Display(tx);

        display.info_block(vec!["first".to_string(), "second".to_string()]);

        let msg = rx.try_recv().expect("message should be sent");
        match msg {
            UiMessage::Info(text) => assert_eq!(text, "first\nsecond"),
            _ => panic!("expected info message"),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn status_sends_details_as_one_transcript_block() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let display = Display(tx);
        let watched = WatchedState {
            state: StateData {
                selected_spec: Some("docs/spec.md".into()),
                selected_milestone: Some("m1.1".into()),
                ..StateData::default()
            },
            state_elapsed: Duration::default(),
            transition: None,
            selected_spec_display: None,
            selected_milestone_display: None,
            selected_milestones: Vec::new(),
        };
        let agents = AgentsRegistry {
            agents: vec![AgentEntry {
                role: ROLE_SUPERVISOR.into(),
                agent_type: "codex".into(),
                name: "supervisor".into(),
                pid: None,
                status: AgentStatus::Suspended,
                started_at: None,
            }],
        };

        display.status(&watched, &agents);

        assert!(matches!(
            rx.try_recv().expect("status update should be sent"),
            UiMessage::StatusUpdate(_)
        ));
        let msg = rx.try_recv().expect("status details should be sent");
        match msg {
            UiMessage::Info(text) => {
                assert!(text.contains("state      : Idle\n"));
                assert!(text.contains("spec       : docs/spec.md\n"));
                assert!(text.contains("milestone  : m1.1\n"));
                assert!(text.contains("  [supervisor] suspended"));
            }
            _ => panic!("expected info message"),
        }
        assert!(rx.try_recv().is_err());
    }

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
