mod commands;
mod display;
mod repl;
mod state_watcher;
pub mod agent_manager; // stub for next task

use anyhow::Result;
use tokio::sync::{mpsc, watch};

use crate::state::{
    agents,
    machine::{StateData, TaskState},
    store,
};
use commands::{parse_command, ShellCommand};

pub async fn run() -> Result<()> {
    display::print_info("ferrus HQ — /status, /plan, /quit, /help");

    let (state_tx, mut state_rx) = watch::channel::<Option<StateData>>(None);
    tokio::spawn(state_watcher::watch(state_tx));

    let (line_tx, mut line_rx) = mpsc::unbounded_channel::<Option<String>>();
    tokio::task::spawn_blocking(move || repl::readline_loop(line_tx));

    let mut ctx = HqContext::new();
    let mut last_state: Option<StateData> = None;

    loop {
        tokio::select! {
            _ = state_rx.changed() => {
                if let Some(new) = state_rx.borrow().clone() {
                    let prev_task_state = last_state.as_ref().map(|s| &s.state);
                    if prev_task_state != Some(&new.state) {
                        if let Some(prev) = prev_task_state {
                            display::print_transition(prev, &new.state);
                        }
                        ctx.on_state_change(&new).await;
                    }
                    last_state = Some(new);
                }
            }
            msg = line_rx.recv() => {
                match msg {
                    Some(Some(line)) if !line.is_empty() => {
                        if let Err(e) = dispatch(&line, &mut ctx).await {
                            display::print_error(&e.to_string());
                        }
                    }
                    Some(Some(_)) => {} // empty line
                    _ => {
                        display::print_info("Bye.");
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

async fn dispatch(line: &str, ctx: &mut HqContext) -> Result<()> {
    match parse_command(line)? {
        ShellCommand::Quit => {
            display::print_info("Bye.");
            std::process::exit(0);
        }
        ShellCommand::Status => {
            let state = store::read_state().await?;
            let reg = agents::read_agents().await?;
            display::print_status(&state, &reg);
        }
        ShellCommand::Plan => ctx.plan().await?,
        ShellCommand::Attach { name } => {
            display::print_info(&format!("/attach {name} — available in Phase B (PTY)"));
        }
        ShellCommand::Init { agents_path } => {
            crate::cli::commands::init::run(agents_path).await?;
        }
        ShellCommand::Register { supervisor, executor } => {
            let sup = supervisor.as_deref().and_then(parse_agent_type);
            let exe = executor.as_deref().and_then(parse_agent_type);
            if sup.is_none() && exe.is_none() {
                display::print_error("At least one of --supervisor or --executor required");
            } else {
                crate::cli::commands::register::run(sup, exe).await?;
            }
        }
    }
    Ok(())
}

fn parse_agent_type(s: &str) -> Option<crate::cli::commands::register::Agent> {
    use crate::cli::commands::register::Agent;
    match s { "claude-code" => Some(Agent::ClaudeCode), "codex" => Some(Agent::Codex), _ => None }
}

// --- HqContext ---

pub(crate) struct HqContext {
    _private: (),
}

impl HqContext {
    fn new() -> Self { Self { _private: () } }

    async fn plan(&mut self) -> Result<()> {
        display::print_info("/plan — implemented in Task 5");
        Ok(())
    }

    async fn on_state_change(&mut self, _state: &StateData) {
        // Orchestration routing — implemented in Task 5
    }
}

// --- Transition routing ---

#[allow(dead_code)]
#[derive(Debug, PartialEq)]
pub(crate) enum TransitionAction {
    SpawnExecutor,
    SpawnReviewer,
    KillReviewerSpawnExecutor,
    TaskComplete,
    TaskFailed,
    NoOp,
}

#[allow(dead_code)]
pub(crate) fn transition_action(from: &TaskState, to: &TaskState) -> TransitionAction {
    use TaskState::*;
    match (from, to) {
        (Idle, Executing) => TransitionAction::SpawnExecutor,
        (Executing | Addressing | Checking, Reviewing) => TransitionAction::SpawnReviewer,
        (Reviewing, Addressing) => TransitionAction::KillReviewerSpawnExecutor,
        (Reviewing, Complete) => TransitionAction::TaskComplete,
        (_, Failed) => TransitionAction::TaskFailed,
        _ => TransitionAction::NoOp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use TaskState::*;

    #[test]
    fn idle_to_executing_spawns_executor() {
        assert_eq!(transition_action(&Idle, &Executing), TransitionAction::SpawnExecutor);
    }
    #[test]
    fn executing_to_reviewing_spawns_reviewer() {
        assert_eq!(transition_action(&Executing, &Reviewing), TransitionAction::SpawnReviewer);
    }
    #[test]
    fn reviewing_to_addressing_kills_reviewer_spawns_executor() {
        assert_eq!(transition_action(&Reviewing, &Addressing), TransitionAction::KillReviewerSpawnExecutor);
    }
    #[test]
    fn reviewing_to_complete() {
        assert_eq!(transition_action(&Reviewing, &Complete), TransitionAction::TaskComplete);
    }
    #[test]
    fn any_to_failed() {
        assert_eq!(transition_action(&Executing, &Failed), TransitionAction::TaskFailed);
    }
    #[test]
    fn executing_to_checking_is_noop() {
        assert_eq!(transition_action(&Executing, &Checking), TransitionAction::NoOp);
    }
}
