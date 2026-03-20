pub mod agent_manager;
mod commands;
mod display;
mod repl;
mod state_watcher; // stub for next task

use anyhow::{Context, Result};
use std::io::Write;
use tokio::sync::watch;

use crate::state::{
    agents,
    machine::{StateData, TaskState},
    store,
};
use commands::{parse_command, ShellCommand};

pub async fn run() -> Result<()> {
    use rustyline::DefaultEditor;

    reconcile_agent_pids().await;
    display::print_info("ferrus HQ — /status, /reset, /plan, /attach <name>, /quit, /help");

    let (state_tx, state_rx) = watch::channel::<Option<StateData>>(None);
    tokio::spawn(state_watcher::watch(state_tx));

    let mut ctx = HqContext::new(state_rx);
    // DefaultEditor is !Send — use block_in_place (same thread) rather than spawn_blocking.
    let mut rl = DefaultEditor::new()?;

    loop {
        // Print any state transitions that arrived while we were waiting.
        ctx.drain_state_changes().await;

        let prompt = repl::hq_prompt().to_string();
        // block_in_place: runs blocking readline on the current thread without spawning.
        // rl stays in scope — no Send requirement, no move-in/move-out dance.
        let line = tokio::task::block_in_place(|| repl::readline_once(&mut rl, &prompt));

        match line {
            Some(l) if !l.is_empty() => {
                if let Err(e) = dispatch(&l, &mut ctx).await {
                    display::print_error(&e.to_string());
                }
            }
            Some(_) => {} // blank line or Ctrl-C
            None => {
                display::print_info("Bye.");
                break;
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
            if !ctx.sessions.is_empty() {
                display::print_info("PTY sessions:");
                for (name, session) in &ctx.sessions {
                    let status = if session.is_alive() {
                        "running"
                    } else {
                        "exited"
                    };
                    display::print_info(&format!(
                        "  {name} ({status}) — /attach {name} — logs: {}",
                        session.log_path.display(),
                    ));
                }
            }
        }
        ShellCommand::Reset => ctx.reset().await?,
        ShellCommand::Plan => ctx.plan().await?,
        ShellCommand::Review => ctx.review().await?,
        ShellCommand::Attach { name } => {
            if let Some(session) = ctx.sessions.get(&name) {
                display::print_info(&format!("Attaching to {name}. Ctrl-B d to detach.",));
                match session.attach().await {
                    Ok(crate::pty::DetachReason::UserDetach) => display::print_info(&format!(
                        "Detached from {name}. Use /attach {name} to reconnect."
                    )),
                    Ok(crate::pty::DetachReason::ProcessExit) => {
                        display::print_info(&format!("{name} process exited."));
                        ctx.sessions.remove(&name);
                    }
                    Err(e) => display::print_error(&format!("Attach error: {e}")),
                }
            } else {
                display::print_error(&format!(
                    "No session named '{name}'. Run /status to see active sessions."
                ));
            }
        }
        ShellCommand::Init { agents_path } => {
            crate::cli::commands::init::run(agents_path).await?;
        }
        ShellCommand::Register {
            supervisor,
            executor,
        } => {
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
    match s {
        "claude-code" => Some(Agent::ClaudeCode),
        "codex" => Some(Agent::Codex),
        _ => None,
    }
}

// --- HqContext ---

pub(crate) struct HqContext {
    pub(crate) supervisor_type: Option<String>,
    pub(crate) executor_type: Option<String>,
    /// Active background PTY sessions, keyed by name (e.g. "executor-1").
    pub(crate) sessions: std::collections::HashMap<String, crate::pty::BackgroundSession>,
    /// Last observed task state (for transition detection in on_state_change).
    pub(crate) last_task_state: Option<crate::state::machine::TaskState>,
    /// State watcher receiver — drained before each readline call.
    state_rx: watch::Receiver<Option<StateData>>,
}

impl HqContext {
    fn new(state_rx: watch::Receiver<Option<StateData>>) -> Self {
        Self {
            supervisor_type: None,
            executor_type: None,
            sessions: std::collections::HashMap::new(),
            last_task_state: None,
            state_rx,
        }
    }

    /// Drain any state changes that arrived since the last readline call.
    /// Prints transition banners and triggers on_state_change without blocking.
    pub(crate) async fn drain_state_changes(&mut self) {
        while let Ok(true) = self.state_rx.has_changed() {
            let new = self.state_rx.borrow_and_update().clone();
            if let Some(new_state) = new {
                let prev = self.last_task_state.clone();
                if prev.as_ref() != Some(&new_state.state) {
                    if let Some(ref p) = prev {
                        display::print_transition(p, &new_state.state);
                    }
                    self.on_state_change(&new_state).await;
                }
                self.last_task_state = Some(new_state.state.clone());
            }
        }
    }

    /// Called when STATE.json transitions to a new TaskState.
    /// Phase B: drives automatic spawning of executor/reviewer background sessions.
    ///
    /// # Design note: bootstrap guard
    /// `on_state_change` requires a known previous state to compute `transition_action`.
    /// When `last_task_state` is None (HQ just started or restarted with an active task),
    /// there is no previous state, so we record the current state and return — no spawning.
    /// This prevents a cold-start observation of e.g. `Executing` from being misread as
    /// a fresh Idle→Executing transition that needs a new executor spawned.
    ///
    /// The Idle→Executing transition triggered by `/plan` is handled *explicitly* in
    /// `plan()` via `spawn_background_session` — not via this path.
    ///
    /// TODO(Phase C): `bootstrap_from_state` — when HQ restarts with an active task and
    /// no live session, auto-reattach or prompt the user to resume.
    pub(crate) async fn on_state_change(&mut self, state: &StateData) {
        // Bootstrap guard: first observation records state without spawning anything.
        // Prevents misinterpreting a cold-start observation as a new transition.
        if self.last_task_state.is_none() {
            self.last_task_state = Some(state.state.clone());
            return;
        }
        // Requires last_task_state to compute the transition action.
        let Some(ref prev) = self.last_task_state else {
            return;
        };
        let action = transition_action(prev, &state.state);
        let exe_type = self.executor_type.clone().unwrap_or_else(|| "codex".into());
        let sup_type = self
            .supervisor_type
            .clone()
            .unwrap_or_else(|| "claude-code".into());
        match action {
            TransitionAction::SpawnExecutor => {
                if let Err(e) = self
                    .spawn_background_session(
                        &exe_type,
                        "executor",
                        "executor-1",
                        Some(agent_manager::executor_prompt()),
                    )
                    .await
                {
                    display::print_error(&format!("Failed to spawn executor: {e}"));
                }
            }
            TransitionAction::SpawnReviewer => {
                // Close the executor session before spawning the reviewer.
                // If the executor is still alive (rare), dropping closes the PTY.
                // On Unix this typically sends SIGHUP; not guaranteed on all platforms.
                self.sessions.remove("executor-1");
                if let Err(e) = self
                    .spawn_background_session(
                        &sup_type,
                        "supervisor",
                        "supervisor-1",
                        Some(agent_manager::reviewer_prompt()),
                    )
                    .await
                {
                    display::print_error(&format!("Failed to spawn reviewer: {e}"));
                }
            }
            TransitionAction::KillReviewerSpawnExecutor => {
                // Dropping the session closes the PTY master.
                // On Unix this typically results in SIGHUP to the child,
                // but this is not guaranteed across all platforms or agents.
                self.sessions.remove("supervisor-1");
                if let Err(e) = self
                    .spawn_background_session(
                        &exe_type,
                        "executor",
                        "executor-1",
                        Some(agent_manager::executor_prompt()),
                    )
                    .await
                {
                    display::print_error(&format!("Failed to spawn executor: {e}"));
                }
            }
            TransitionAction::TaskComplete => {
                // Clean up all sessions — task is done.
                self.sessions.remove("executor-1");
                self.sessions.remove("supervisor-1");
                display::print_info("Task complete! Use /plan to start a new task.");
            }
            TransitionAction::TaskFailed => {
                // Clean up all sessions — nothing useful left running.
                self.sessions.remove("executor-1");
                self.sessions.remove("supervisor-1");
                display::print_info("Task failed. Use /status for details, /reset to try again.");
            }
            TransitionAction::NoOp => {}
        }
    }

    /// Spawn a named background PTY session, skipping if one is already alive.
    ///
    /// # Session name contract
    /// Session names (e.g. "executor-1", "supervisor-1") are unique by role. The reuse
    /// check is by name only — callers must ensure the name always maps to the same
    /// role/agent_type combination. This invariant holds for Phase B's fixed roles.
    pub(crate) async fn spawn_background_session(
        &mut self,
        agent_type: &str,
        role: &str,
        name: &str,
        prompt: Option<&str>,
    ) -> Result<()> {
        // Reuse if already alive.
        if let Some(existing) = self.sessions.get(name) {
            if existing.is_alive() {
                display::print_info(&format!("{name} already running."));
                return Ok(());
            }
            self.sessions.remove(name);
        }
        display::print_info(&format!("Spawning {name} ({agent_type}) in background…"));
        let session = agent_manager::spawn_background_pty(agent_type, role, name, prompt).await?;
        display::print_info(&format!(
            "{name} started. Use /attach {name} to observe. Logs: {}",
            session.log_path.display(),
        ));
        self.sessions.insert(name.to_string(), session);
        Ok(())
    }

    /// Manually spawn supervisor in review mode for a pending submission.
    /// Use when automatic reviewer spawning failed or HQ was restarted mid-review.
    async fn review(&mut self) -> Result<()> {
        use crate::config::Config;
        use crate::state::machine::TaskState;

        let state = store::read_state().await?;
        if state.state != TaskState::Reviewing {
            anyhow::bail!(
                "State is {:?} — /review requires Reviewing. Use /status.",
                state.state
            );
        }

        // Use cached type or load from config.
        let sup_type = if let Some(ref t) = self.supervisor_type {
            t.clone()
        } else {
            let config = Config::load().await?;
            let hq = config.hq.ok_or_else(|| {
                anyhow::anyhow!(
                    "No [hq] section in ferrus.toml. Add:\n[hq]\nsupervisor = \"claude-code\"\nexecutor = \"codex\""
                )
            })?;
            self.supervisor_type = Some(hq.supervisor.clone());
            self.executor_type = Some(hq.executor.clone());
            hq.supervisor
        };

        self.spawn_background_session(
            &sup_type,
            "supervisor",
            "supervisor-1",
            Some(agent_manager::reviewer_prompt()),
        )
        .await
    }

    async fn reset(&mut self) -> Result<()> {
        let mut state = store::read_state().await?;
        if matches!(state.state, TaskState::Executing | TaskState::Reviewing) {
            let answer = tokio::task::block_in_place(|| -> String {
                print!(
                    "  Reset while state is {:?} — agents may be running. Continue? [y/N] ",
                    state.state
                );
                let _ = std::io::stdout().flush();
                let mut buf = String::new();
                let _ = std::io::stdin().read_line(&mut buf);
                buf.trim().to_lowercase()
            });

            if !matches!(answer.as_str(), "y" | "yes") {
                display::print_info("Reset cancelled.");
                return Ok(());
            }
        }

        self.sessions.remove("executor-1");
        self.sessions.remove("supervisor-1");

        let mut reg = agents::read_agents().await?;
        for role in ["executor", "supervisor"] {
            if let Some(entry) = reg.by_role_mut(role) {
                entry.pid = None;
                entry.status = agents::AgentStatus::Suspended;
            }
        }
        agents::write_agents(&reg).await?;

        store::clear_task().await?;
        store::clear_submission().await?;
        store::clear_answer().await?;
        store::clear_feedback().await?;
        store::clear_question().await?;
        store::clear_review().await?;

        state.force_reset();
        store::write_state(&state).await?;

        self.last_task_state = Some(TaskState::Idle);
        display::print_info("State reset to Idle. All task files cleared.");
        Ok(())
    }

    /// Planning flow: spawn supervisor interactively, then kill it automatically as soon as
    /// /create_task transitions the state to Executing.
    async fn plan(&mut self) -> Result<()> {
        use crate::config::Config;
        use crate::state::machine::TaskState;
        use std::process::Stdio;
        use tokio::process::Command;

        let config = Config::load().await?;
        let hq = config.hq.ok_or_else(|| {
            anyhow::anyhow!(
                "No [hq] section in ferrus.toml. Add:\n[hq]\nsupervisor = \"claude-code\"\nexecutor = \"codex\""
            )
        })?;

        let state = store::read_state().await?;
        if state.state != TaskState::Idle {
            anyhow::bail!(
                "State is {:?} — /plan requires Idle. Use /status.",
                state.state
            );
        }

        self.supervisor_type = Some(hq.supervisor.clone());
        self.executor_type = Some(hq.executor.clone());

        display::print_info(&format!("Spawning supervisor ({})…", hq.supervisor));
        display::print_info("Collaborate with the supervisor to define the task.");

        let binary = agent_manager::agent_binary(&hq.supervisor);
        let prompt = agent_manager::supervisor_plan_prompt();

        let mut child = Command::new(binary)
            .arg(prompt)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("Failed to spawn {binary}"))?;

        // Register in agents.json.
        {
            use agents::{read_agents, write_agents, AgentEntry, AgentStatus};
            let mut reg = read_agents().await?;
            reg.upsert(AgentEntry {
                role: "supervisor".into(),
                agent_type: hq.supervisor.clone(),
                name: "supervisor-1".into(),
                pid: child.id(),
                status: AgentStatus::Running,
                started_at: Some(chrono::Utc::now()),
            });
            write_agents(&reg).await?;
        }

        // Poll STATE.json every 300 ms while the supervisor runs.
        // As soon as /create_task moves state to Executing, kill the supervisor
        // and let HQ take over — no need for the user to manually exit.
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(300));
        loop {
            tokio::select! {
                _ = child.wait() => break, // exited naturally
                _ = ticker.tick() => {
                    if let Ok(s) = store::read_state().await {
                        if s.state == TaskState::Executing {
                            display::print_info("Task created — stopping supervisor…");
                            let _ = child.kill().await;
                            let _ = child.wait().await;
                            break;
                        }
                    }
                }
            }
        }

        // Mark supervisor as Suspended in agents.json.
        {
            use agents::{read_agents, write_agents, AgentStatus};
            let mut reg = read_agents().await?;
            if let Some(e) = reg.by_role_mut("supervisor") {
                e.pid = None;
                e.status = AgentStatus::Suspended;
            }
            write_agents(&reg).await?;
        }

        // Spawn executor if task was created.
        let new_state = store::read_state().await?;
        if new_state.state == TaskState::Executing {
            display::print_info("Spawning executor in background…");
            self.spawn_background_session(
                &hq.executor,
                "executor",
                "executor-1",
                Some(agent_manager::executor_prompt()),
            )
            .await?;
            display::print_info(
                "Executor running. State changes print automatically. Use /attach executor-1 to observe.",
            );
        } else {
            display::print_info(&format!(
                "No task created (state is {:?}). Re-run /plan when ready.",
                new_state.state
            ));
        }
        Ok(())
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
        if ret == 0 {
            return true;
        }
        let errno = unsafe { *libc::__errno_location() };
        errno == libc::EPERM
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// On startup, mark any Running entries whose PID is no longer alive as Suspended.
async fn reconcile_agent_pids() {
    use crate::state::agents::{read_agents, write_agents, AgentStatus};
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
        if changed {
            let _ = write_agents(&reg).await;
        }
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
        assert_eq!(
            transition_action(&Idle, &Executing),
            TransitionAction::SpawnExecutor
        );
    }
    #[test]
    fn executing_to_reviewing_spawns_reviewer() {
        assert_eq!(
            transition_action(&Executing, &Reviewing),
            TransitionAction::SpawnReviewer
        );
    }
    #[test]
    fn reviewing_to_addressing_kills_reviewer_spawns_executor() {
        assert_eq!(
            transition_action(&Reviewing, &Addressing),
            TransitionAction::KillReviewerSpawnExecutor
        );
    }
    #[test]
    fn reviewing_to_complete() {
        assert_eq!(
            transition_action(&Reviewing, &Complete),
            TransitionAction::TaskComplete
        );
    }
    #[test]
    fn any_to_failed() {
        assert_eq!(
            transition_action(&Executing, &Failed),
            TransitionAction::TaskFailed
        );
    }
    #[test]
    fn executing_to_checking_is_noop() {
        assert_eq!(
            transition_action(&Executing, &Checking),
            TransitionAction::NoOp
        );
    }
    #[test]
    fn stale_pid_detection() {
        // The current process is always alive — solid invariant on all Unix.
        assert!(pid_is_alive(std::process::id()));
        // 999999 is virtually guaranteed not to be a live PID.
        assert!(!pid_is_alive(999999));
    }
}
