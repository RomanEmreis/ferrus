pub mod agent_manager;
mod commands;
mod display;
mod state_watcher;
mod tui;

use anyhow::{Context, Result};
use tokio::process::Command;
use tokio::sync::watch;

use crate::state::{
    agents,
    machine::{StateData, TaskState},
    store,
};
use commands::{parse_command, ShellCommand};
use display::Display;

pub async fn run() -> Result<()> {
    reconcile_agent_pids().await;

    let (state_tx, state_rx) = watch::channel::<Option<StateData>>(None);
    tokio::spawn(state_watcher::watch(state_tx));

    let (msg_tx, msg_rx) = tokio::sync::mpsc::unbounded_channel::<tui::UiMessage>();
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    let (supervisor_type, executor_type) = load_agent_types_from_config().await;
    let (supervisor_version, executor_version) =
        load_agent_versions(&supervisor_type, &executor_type).await;

    let display = Display(msg_tx);
    let mut ctx = HqContext::new(state_rx.clone(), display.clone());
    ctx.supervisor_type = (!supervisor_type.is_empty()).then_some(supervisor_type.clone());
    ctx.executor_type = (!executor_type.is_empty()).then_some(executor_type.clone());

    let mut tui_task = tokio::spawn(tui::run_tui(
        msg_rx,
        cmd_tx,
        state_rx.clone(),
        supervisor_type,
        executor_type,
        supervisor_version,
        executor_version,
    ));

    loop {
        tokio::select! {
            changed = ctx.state_rx.changed() => {
                if changed.is_ok() {
                    let snap = ctx.state_rx.borrow_and_update().clone();
                    if let Some(new_state) = snap {
                        let prev = ctx.last_task_state.clone();
                        if prev.as_ref() != Some(&new_state.state) {
                            if let Some(ref previous) = prev {
                                ctx.display.transition(previous, &new_state.state);
                            }
                            ctx.on_state_change(&new_state).await;
                        }
                        ctx.last_task_state = Some(new_state.state.clone());
                    }
                } else {
                    break;
                }
            }
            maybe_cmd = cmd_rx.recv() => {
                match maybe_cmd {
                    Some(cmd) => {
                        let line = cmd.trim();
                        if line.is_empty() {
                            continue;
                        }
                        if line == "/quit" {
                            ctx.display.info("Bye.");
                            break;
                        }
                        if let Err(err) = dispatch(line, &mut ctx).await {
                            ctx.display.error(err.to_string());
                        }
                    }
                    None => break,
                }
            }
            result = &mut tui_task => {
                return result?;
            }
        }
    }

    drop(ctx);
    match tui_task.await {
        Ok(result) => result?,
        Err(err) if err.is_cancelled() => {}
        Err(err) => return Err(err.into()),
    }

    Ok(())
}

async fn load_agent_types_from_config() -> (String, String) {
    use crate::config::Config;

    if let Ok(cfg) = Config::load().await {
        if let Some(hq) = cfg.hq {
            return (hq.supervisor, hq.executor);
        }
    }
    (String::new(), String::new())
}

async fn load_agent_versions(supervisor_type: &str, executor_type: &str) -> (String, String) {
    let supervisor = load_agent_version(supervisor_type).await;
    let executor = load_agent_version(executor_type).await;
    (supervisor, executor)
}

async fn load_agent_version(agent_type: &str) -> String {
    if agent_type.is_empty() {
        return String::new();
    }

    let binary = agent_manager::agent_binary(agent_type);
    let Ok(output) = Command::new(binary).arg("--version").output().await else {
        return String::new();
    };
    if !output.status.success() {
        return String::new();
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

async fn dispatch(line: &str, ctx: &mut HqContext) -> Result<()> {
    match parse_command(line)? {
        ShellCommand::Quit => {
            ctx.display.info("Bye.");
        }
        ShellCommand::Status => {
            let state = store::read_state().await?;
            let reg = agents::read_agents().await?;
            ctx.display.status(&state, &reg);
            if !ctx.sessions.is_empty() {
                ctx.display.info("PTY sessions:");
                for (name, session) in &ctx.sessions {
                    let status = if session.is_alive() {
                        "running"
                    } else {
                        "exited"
                    };
                    ctx.display.info(format!(
                        "  {name} ({status}) — /attach {name} — logs: {}",
                        session.log_path.display()
                    ));
                }
            }
        }
        ShellCommand::Help => {
            ctx.display.info(concat!(
                "ferrus HQ commands:\n",
                "  /plan              Spawn supervisor, plan a task, then run executor→review loop\n",
                "  /execute           Start or resume the executor for the current task\n",
                "  /review            Manually spawn supervisor in review mode\n",
                "  /status            Show task state, agent list, and session log paths\n",
                "  /attach <name>     Attach to a running background session (Ctrl+] d to detach)\n",
                "  /stop              Stop all running agent sessions\n",
                "  /reset             Reset state to Idle (clears task files)\n",
                "  /init              Initialize ferrus in the current directory\n",
                "  /register          Register agent configs (.mcp.json / .codex/config.toml)\n",
                "  /quit              Exit HQ",
            ));
        }
        ShellCommand::Reset => ctx.reset().await?,
        ShellCommand::Stop => ctx.stop().await?,
        ShellCommand::Plan => ctx.plan().await?,
        ShellCommand::Execute => ctx.execute().await?,
        ShellCommand::Review => ctx.review().await?,
        ShellCommand::Attach { name } => {
            if let Some(session) = ctx.sessions.get(&name) {
                let force_detach = std::sync::Arc::clone(&session.force_detach);
                let mut state_rx_clone = ctx.state_rx.clone();
                let is_executor = name.starts_with("executor");
                let watcher = tokio::spawn(async move {
                    use crate::state::machine::TaskState;

                    loop {
                        if state_rx_clone.changed().await.is_err() {
                            break;
                        }
                        let done = state_rx_clone.borrow().as_ref().is_some_and(|sd| {
                            if is_executor {
                                !matches!(
                                    sd.state,
                                    TaskState::Executing
                                        | TaskState::Addressing
                                        | TaskState::Checking
                                )
                            } else {
                                sd.state != TaskState::Reviewing
                            }
                        });
                        if done {
                            force_detach.notify_one();
                            break;
                        }
                    }
                });

                ctx.display
                    .info(format!("Attaching to {name}. Ctrl+] d to detach."));
                let ack_rx = ctx.display.suspend();
                let _ = ack_rx.await;
                let result = session.attach().await;
                ctx.display.resume();
                match result {
                    Ok(crate::pty::DetachReason::UserDetach) => ctx
                        .display
                        .info(format!("Detached from {name}. Use /attach {name} to reconnect.")),
                    Ok(crate::pty::DetachReason::ProcessExit) => {
                        ctx.display.info(format!("{name} process exited."));
                        ctx.sessions.remove(&name);
                    }
                    Ok(crate::pty::DetachReason::AutoDetach) => ctx.display.info(format!(
                        "{name} task is done — returning to HQ. Agent may still be running; use /attach {name} to reconnect."
                    )),
                    Err(err) => ctx.display.error(format!("Attach error: {err}")),
                }
                watcher.abort();
            } else {
                ctx.display.error(format!(
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
                ctx.display
                    .error("At least one of --supervisor or --executor required");
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

pub(crate) struct HqContext {
    pub(crate) supervisor_type: Option<String>,
    pub(crate) executor_type: Option<String>,
    pub(crate) sessions: std::collections::HashMap<String, crate::pty::BackgroundSession>,
    pub(crate) last_task_state: Option<TaskState>,
    state_rx: watch::Receiver<Option<StateData>>,
    pub(crate) display: Display,
}

impl HqContext {
    fn new(state_rx: watch::Receiver<Option<StateData>>, display: Display) -> Self {
        Self {
            supervisor_type: None,
            executor_type: None,
            sessions: std::collections::HashMap::new(),
            last_task_state: None,
            state_rx,
            display,
        }
    }

    pub(crate) async fn on_state_change(&mut self, state: &StateData) {
        if self.last_task_state.is_none() {
            self.last_task_state = Some(state.state.clone());
            return;
        }
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
                if let Err(err) = self
                    .spawn_background_session(
                        &exe_type,
                        "executor",
                        "executor-1",
                        Some(agent_manager::executor_prompt()),
                    )
                    .await
                {
                    self.display
                        .error(format!("Failed to spawn executor: {err}"));
                }
            }
            TransitionAction::SpawnReviewer => {
                self.sessions.remove("executor-1");
                if let Err(err) = self
                    .spawn_background_session(
                        &sup_type,
                        "supervisor",
                        "supervisor-1",
                        Some(agent_manager::reviewer_prompt()),
                    )
                    .await
                {
                    self.display
                        .error(format!("Failed to spawn reviewer: {err}"));
                }
            }
            TransitionAction::KillReviewerSpawnExecutor => {
                self.sessions.remove("supervisor-1");
                if let Err(err) = self
                    .spawn_background_session(
                        &exe_type,
                        "executor",
                        "executor-1",
                        Some(agent_manager::executor_prompt()),
                    )
                    .await
                {
                    self.display
                        .error(format!("Failed to spawn executor: {err}"));
                }
            }
            TransitionAction::TaskComplete => {
                self.sessions.remove("executor-1");
                self.sessions.remove("supervisor-1");
                self.display
                    .info("Task complete! Use /plan to start a new task.");
            }
            TransitionAction::TaskFailed => {
                self.sessions.remove("executor-1");
                self.sessions.remove("supervisor-1");
                self.display
                    .info("Task failed. Use /status for details, /reset to try again.");
            }
            TransitionAction::NoOp => {}
        }
    }

    pub(crate) async fn spawn_background_session(
        &mut self,
        agent_type: &str,
        role: &str,
        name: &str,
        prompt: Option<&str>,
    ) -> Result<()> {
        if let Some(existing) = self.sessions.get(name) {
            if existing.is_alive() {
                self.display.info(format!("{name} already running."));
                return Ok(());
            }
            self.sessions.remove(name);
        }

        self.display
            .info(format!("Spawning {name} ({agent_type}) in background…"));
        let session = agent_manager::spawn_background_pty(agent_type, role, name, prompt).await?;
        self.display.info(format!(
            "{name} started. Use /attach {name} to observe. Logs: {}",
            session.log_path.display()
        ));
        self.sessions.insert(name.to_string(), session);
        Ok(())
    }

    async fn execute(&mut self) -> Result<()> {
        use crate::config::Config;

        if self
            .sessions
            .iter()
            .any(|(name, session)| name.starts_with("executor") && session.is_alive())
        {
            self.display.info(
                "An executor is already running — work is in progress. Plan a new task first with /plan.",
            );
            return Ok(());
        }

        let state = store::read_state().await?;
        match state.state {
            TaskState::Complete => {
                self.display
                    .info("Task is already complete. Use /plan to start a new task.");
                return Ok(());
            }
            TaskState::Reviewing => {
                self.display.info(
                    "Execution is done and submission is pending review. Use /review to review it.",
                );
                return Ok(());
            }
            _ => {}
        }

        let exe_type = if let Some(ref t) = self.executor_type {
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
            hq.executor
        };

        self.spawn_background_session(
            &exe_type,
            "executor",
            "executor-1",
            Some(agent_manager::executor_prompt()),
        )
        .await
    }

    async fn review(&mut self) -> Result<()> {
        use crate::config::Config;

        let state = store::read_state().await?;
        if state.state != TaskState::Reviewing {
            anyhow::bail!(
                "State is {:?} — /review requires Reviewing. Use /status.",
                state.state
            );
        }

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
        self.do_reset(true).await
    }

    async fn do_reset(&mut self, prompt: bool) -> Result<()> {
        let mut state = store::read_state().await?;
        if prompt && matches!(state.state, TaskState::Executing | TaskState::Reviewing) {
            let reply_rx = self.display.confirm(format!(
                "Reset while state is {:?} — agents may be running. Continue?",
                state.state
            ));
            let confirmed = reply_rx.await.unwrap_or(false);
            if !confirmed {
                self.display.info("Reset cancelled.");
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
        self.display
            .info("State reset to Idle. All task files cleared.");
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        let reply_rx = self.display.confirm("Stop all running agents?");
        let confirmed = reply_rx.await.unwrap_or(false);
        if !confirmed {
            self.display.info("Stop cancelled.");
            return Ok(());
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

        self.display.info("All agent sessions stopped.");
        Ok(())
    }

    async fn plan(&mut self) -> Result<()> {
        use crate::config::Config;
        use std::process::Stdio;
        use tokio::process::Command;

        struct ResumeGuard {
            display: Display,
            active: bool,
        }

        impl ResumeGuard {
            fn new(display: Display) -> Self {
                Self {
                    display,
                    active: true,
                }
            }

            fn resume_now(&mut self) {
                if self.active {
                    self.display.resume();
                    self.active = false;
                }
            }
        }

        impl Drop for ResumeGuard {
            fn drop(&mut self) {
                self.resume_now();
            }
        }

        let config = Config::load().await?;
        let hq = config.hq.ok_or_else(|| {
            anyhow::anyhow!(
                "No [hq] section in ferrus.toml. Add:\n[hq]\nsupervisor = \"claude-code\"\nexecutor = \"codex\""
            )
        })?;

        let state = store::read_state().await?;
        match state.state {
            TaskState::Idle => {}
            TaskState::Complete => {
                self.display
                    .info("Previous task complete — resetting for new task.");
                self.do_reset(false).await?;
            }
            other => {
                anyhow::bail!(
                    "State is {other:?} — /plan requires Idle or Complete. Use /reset first if needed."
                );
            }
        }

        self.supervisor_type = Some(hq.supervisor.clone());
        self.executor_type = Some(hq.executor.clone());

        self.display
            .info(format!("Spawning supervisor ({})…", hq.supervisor));
        self.display
            .info("Collaborate with the supervisor to define the task.");

        let binary = agent_manager::agent_binary(&hq.supervisor);
        let prompt = agent_manager::supervisor_plan_prompt();
        let plan_args = agent_manager::plan_mode_args(&hq.supervisor);

        let mut cmd = Command::new(binary);
        for arg in plan_args {
            cmd.arg(arg);
        }
        cmd.arg(prompt);

        let ack_rx = self.display.suspend();
        let _ = ack_rx.await;
        let mut resume_guard = ResumeGuard::new(self.display.clone());
        let mut child = cmd
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("Failed to spawn {binary}"))?;

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

        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(300));
        loop {
            tokio::select! {
                _ = child.wait() => break,
                _ = ticker.tick() => {
                    if let Ok(s) = store::read_state().await {
                        if s.state == TaskState::Executing {
                            self.display.info("Task created — stopping supervisor…");
                            let _ = child.kill().await;
                            let _ = child.wait().await;
                            break;
                        }
                    }
                }
            }
        }
        resume_guard.resume_now();

        {
            use agents::{read_agents, write_agents, AgentStatus};

            let mut reg = read_agents().await?;
            if let Some(entry) = reg.by_role_mut("supervisor") {
                entry.pid = None;
                entry.status = AgentStatus::Suspended;
            }
            write_agents(&reg).await?;
        }

        let new_state = store::read_state().await?;
        if new_state.state == TaskState::Executing {
            self.display.info("Spawning executor in background…");
            self.spawn_background_session(
                &hq.executor,
                "executor",
                "executor-1",
                Some(agent_manager::executor_prompt()),
            )
            .await?;
            self.display.info(
                "Executor running. State changes print automatically. Use /attach executor-1 to observe.",
            );
        } else {
            self.display.info(format!(
                "No task created (state is {:?}). Re-run /plan when ready.",
                new_state.state
            ));
        }
        Ok(())
    }
}

pub(crate) fn pid_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
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
        assert!(pid_is_alive(std::process::id()));
        assert!(!pid_is_alive(999999));
    }
}
