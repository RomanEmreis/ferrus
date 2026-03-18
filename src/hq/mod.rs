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
    reconcile_agent_pids().await;
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
    supervisor_type: Option<String>,
    executor_type: Option<String>,
}

impl HqContext {
    fn new() -> Self { Self { supervisor_type: None, executor_type: None } }

    async fn plan(&mut self) -> Result<()> {
        use crate::config::Config;
        use crate::state::{machine::TaskState, store};

        let config = Config::load().await?;
        let hq = config.hq.ok_or_else(|| anyhow::anyhow!(
            "No [hq] section in ferrus.toml. Add:\n[hq]\nsupervisor = \"claude-code\"\nexecutor = \"codex\""
        ))?;

        let state = store::read_state().await?;
        if state.state != TaskState::Idle {
            anyhow::bail!("State is {:?} — /plan requires Idle. Use /status.", state.state);
        }

        self.supervisor_type = Some(hq.supervisor.clone());
        self.executor_type = Some(hq.executor.clone());

        display::print_info(&format!("Spawning supervisor ({})…", hq.supervisor));
        display::print_info("Interact with the supervisor. When done, exit/quit it to return to HQ.");

        agent_manager::spawn_and_wait(
            &hq.supervisor, "supervisor", "supervisor-1",
            Some(agent_manager::supervisor_plan_prompt()),
        ).await?;

        // Supervisor exited — check if task was created.
        let new_state = store::read_state().await?;
        if new_state.state == TaskState::Executing {
            display::print_info("Task created — spawning executor…");
            self.run_executor_loop().await?;
        } else {
            display::print_info(&format!(
                "No task created (state is still {:?}). Re-run /plan when ready.",
                new_state.state
            ));
        }
        Ok(())
    }

    /// Runs the full executor→reviewer loop synchronously until Complete or Failed.
    ///
    /// This is the core orchestration unit for Phase A. Keep all transition logic
    /// here rather than spreading it across the REPL and watcher — it makes
    /// Phase B replacement clean: swap this method for an async PTY-driven loop
    /// without touching the REPL or display layers.
    async fn run_executor_loop(&mut self) -> Result<()> {
        use crate::state::{machine::TaskState, store};

        let exe_type = self.executor_type.clone().unwrap_or("codex".into());
        let sup_type = self.supervisor_type.clone().unwrap_or("claude-code".into());

        loop {
            let state = store::read_state().await?;
            match state.state {
                TaskState::Executing | TaskState::Addressing => {
                    display::print_info(&format!("Spawning executor ({exe_type})…"));
                    agent_manager::spawn_and_wait(
                        &exe_type, "executor", "executor-1",
                        Some(agent_manager::executor_prompt()),
                    ).await?;
                }
                TaskState::Reviewing => {
                    display::print_info(&format!("Spawning reviewer ({sup_type})…"));
                    agent_manager::spawn_and_wait(
                        &sup_type, "supervisor", "supervisor-1",
                        Some(agent_manager::reviewer_prompt()),
                    ).await?;
                }
                TaskState::Complete => {
                    display::print_info("Task complete! Use /plan to start a new task.");
                    break;
                }
                TaskState::Failed => {
                    display::print_info("Task failed. Use /status for details.");
                    break;
                }
                TaskState::Idle => {
                    display::print_info("State returned to Idle unexpectedly. Exiting loop.");
                    break;
                }
                _other => {
                    // Transient state (Checking, AwaitingHuman) — poll briefly then re-check.
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                }
            }
        }
        Ok(())
    }

    /// Called by the select! loop when STATE.json changes outside of /plan.
    pub async fn on_state_change(&mut self, _state: &StateData) {
        // Phase A: no-op. Phase B will drive automated loop here.
    }
}

// --- PID Reconciliation ---

/// Returns true if a process with the given PID is alive on this system.
pub(crate) fn pid_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // kill(pid, 0): returns 0 → alive; EPERM → alive (no permission to signal,
        // but process exists); ESRCH → dead. Any other error → assume dead.
        let ret = unsafe { libc::kill(pid as i32, 0) };
        if ret == 0 { return true; }
        let errno = unsafe { *libc::__errno_location() };
        errno == libc::EPERM
    }
    #[cfg(not(unix))]
    { let _ = pid; false }
}

/// On startup, mark any Running entries whose PID is no longer alive as Suspended.
async fn reconcile_agent_pids() {
    use crate::state::agents::{AgentStatus, read_agents, write_agents};
    if let Ok(mut reg) = read_agents().await {
        let mut changed = false;
        for entry in &mut reg.agents {
            if entry.status == AgentStatus::Running {
                let alive = entry.pid.map(pid_is_alive).unwrap_or(false);
                if !alive {
                    entry.pid = None;
                    entry.status = AgentStatus::Suspended;
                    changed = true;
                }
            }
        }
        if changed { let _ = write_agents(&reg).await; }
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
    #[test]
    fn stale_pid_detection() {
        // The current process is always alive — solid invariant on all Unix.
        assert!(pid_is_alive(std::process::id()));
        // 999999 is virtually guaranteed not to be a live PID.
        assert!(!pid_is_alive(999999));
    }
}
