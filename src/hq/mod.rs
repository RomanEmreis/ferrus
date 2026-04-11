pub mod agent_manager;
mod commands;
mod display;
mod state_watcher;
mod tui;

use anyhow::{Context, Result};
use tokio::process::Command;
use tokio::sync::watch;

use crate::agent_id::{agent_id, DEFAULT_AGENT_INDEX, ROLE_EXECUTOR, ROLE_SUPERVISOR};
use crate::agents::{ExecutorAgent, SupervisorAgent};
use crate::config::{Config, HqConfig};
use crate::state::{
    agents,
    machine::{StateData, TaskState},
    store,
};
use commands::{parse_command, ShellCommand};
use display::Display;
use state_watcher::WatchedState;

pub async fn run(debug: bool) -> Result<()> {
    reconcile_agent_pids().await;

    let (state_tx, state_rx) = watch::channel::<Option<WatchedState>>(None);
    tokio::spawn(state_watcher::watch(state_tx));

    let (msg_tx, msg_rx) = tokio::sync::mpsc::unbounded_channel::<tui::UiMessage>();
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    let hq_config = load_hq_config_from_config().await;
    let supervisor_type = hq_config
        .as_ref()
        .map(|hq| hq.supervisor_name().to_string())
        .unwrap_or_default();
    let executor_type = hq_config
        .as_ref()
        .map(|hq| hq.executor_name().to_string())
        .unwrap_or_default();
    let (supervisor_version, executor_version) = load_agent_versions(hq_config.as_ref()).await;

    let display = Display(msg_tx);
    let mut ctx = HqContext::new(state_rx.clone(), display.clone(), debug);
    if let Some(hq) = hq_config {
        ctx.set_hq_config(&hq);
    }

    let mut tui_task = tokio::spawn(tui::run_tui(
        msg_rx,
        cmd_tx,
        state_rx.clone(),
        supervisor_type,
        executor_type,
        supervisor_version,
        executor_version,
    ));

    let mut tui_finished = false;
    let loop_result: Result<()> = loop {
        tokio::select! {
            changed = ctx.state_rx.changed() => {
                if changed.is_ok() {
                    let snap = ctx.state_rx.borrow_and_update().clone();
                    if let Some(watched) = snap {
                        let prev = ctx.last_task_state.clone();
                        if prev.as_ref() != Some(&watched.state.state) {
                            if let Some(ref transition) = watched.transition {
                                ctx.display.transition(transition);
                            }
                            ctx.on_state_change(&watched.state).await;
                        }
                        ctx.last_task_state = Some(watched.state.state.clone());
                    }
                } else {
                    break Ok(());
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
                            break Ok(());
                        }
                        if let Err(err) = dispatch(line, &mut ctx).await {
                            ctx.display.error(err.to_string());
                        }
                    }
                    None => break Ok(()),
                }
            }
            result = &mut tui_task => {
                tui_finished = true;
                break match result {
                    Ok(inner) => inner,
                    Err(err) => Err(err.into()),
                };
            }
        }
    };

    ctx.shutdown_all_headless().await;

    drop(ctx);
    if !tui_finished {
        match tui_task.await {
            Ok(result) => result?,
            Err(err) if err.is_cancelled() => {}
            Err(err) => return Err(err.into()),
        }
    }

    loop_result?;
    Ok(())
}

async fn load_hq_config_from_config() -> Option<HqConfig> {
    Config::load().await.ok().and_then(|cfg| cfg.hq)
}

async fn load_agent_versions(hq: Option<&HqConfig>) -> (String, String) {
    let Some(hq) = hq else {
        return (String::new(), String::new());
    };
    let supervisor = load_agent_version_from_command(hq.supervisor.spawn(None)).await;
    let executor = load_agent_version_from_command(hq.executor.spawn(None)).await;
    (supervisor, executor)
}

async fn load_agent_version_from_command(command: std::process::Command) -> String {
    let program = command.get_program().to_owned();
    let Ok(output) = Command::new(program).arg("--version").output().await else {
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
    // When state is AwaitingHuman, non-command input is treated as the human's answer.
    if !line.starts_with('/') {
        let state = store::read_state().await?;
        if state.state == TaskState::AwaitingHuman {
            return ctx.answer(line.to_string()).await;
        }
        anyhow::bail!("Commands must start with '/' — try /status, /task, /quit");
    }

    match parse_command(line)? {
        ShellCommand::Quit => {
            ctx.display.info("Bye.");
        }
        ShellCommand::Status => {
            let reg = agents::read_agents().await?;
            let watched = if let Some(watched) = ctx.state_rx.borrow().clone() {
                watched
            } else {
                let state = store::read_state().await?;
                WatchedState {
                    state,
                    state_elapsed: std::time::Duration::default(),
                    transition: None,
                }
            };
            ctx.display.status(&watched, &reg);
            if !ctx.headless.is_empty() {
                ctx.display.info("Headless agents:");
                for (name, handle) in &ctx.headless {
                    let status = if handle.is_alive() {
                        "running"
                    } else {
                        "exited"
                    };
                    ctx.display.info(format!(
                        "  {name} ({status}) — tail logs: {}",
                        handle.log_path.display()
                    ));
                }
            }
        }
        ShellCommand::Help => {
            ctx.display.info(concat!(
                "ferrus HQ commands:\n",
                "  /plan              Free-form planning session with the supervisor\n",
                "  /task              Define a task with the supervisor, then run executor→review loop\n",
                "  /supervisor        Open an interactive supervisor session\n",
                "  /executor          Open an interactive executor session\n",
                "  /resume            Resume the executor headlessly; recovers Consultation too\n",
                "  /review            Manually spawn supervisor in review mode\n",
                "  /status            Show task state, agent list, and session log paths\n",
                "  /attach <name>     Show log path for a running headless agent\n",
                "  /stop              Stop all running agent sessions\n",
                "  /reset             Reset state to Idle (clears task files)\n",
                "  /init              Initialize ferrus in the current directory\n",
                "  /register          Register agent configs (.mcp.json / .codex/config.toml)\n",
                "  /quit              Exit HQ\n",
                "\n",
                "When an agent asks a question (state = AwaitingHuman):\n",
                "  Type your answer and press Enter (no slash prefix needed).",
            ));
        }
        ShellCommand::Reset => ctx.reset().await?,
        ShellCommand::Stop => ctx.stop().await?,
        ShellCommand::Plan => ctx.plan().await?,
        ShellCommand::Task => ctx.task().await?,
        ShellCommand::Supervisor => ctx.supervisor_interactive().await?,
        ShellCommand::Executor => ctx.executor_interactive().await?,
        ShellCommand::Resume => ctx.resume().await?,
        ShellCommand::Review => ctx.review().await?,
        ShellCommand::Attach { name } => {
            if let Some(handle) = ctx.headless.get(&name) {
                let log = handle.log_path.display().to_string();
                ctx.display.info(format!(
                    "{name} runs headlessly — no terminal to attach.\n\
                     Tail its log to observe: tail -f {log}"
                ));
            } else {
                ctx.display.error(format!(
                    "No agent named '{name}'. Run /status to see active agents."
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

struct TerminalGuard {
    #[cfg(unix)]
    original_pgid: libc::pid_t,
    #[cfg(unix)]
    signal_fd: libc::c_int,
    #[cfg(unix)]
    signal_handler: libc::sighandler_t,
}

impl TerminalGuard {
    fn for_child(pid: u32) -> Self {
        #[cfg(unix)]
        unsafe {
            let signal_fd = libc::STDIN_FILENO;
            let original_pgid = libc::tcgetpgrp(signal_fd);
            let signal_handler = libc::signal(libc::SIGTTOU, libc::SIG_IGN);
            let _ = libc::tcsetpgrp(signal_fd, pid as libc::pid_t);
            Self {
                original_pgid,
                signal_fd,
                signal_handler,
            }
        }

        #[cfg(not(unix))]
        {
            let _ = pid;
            Self {}
        }
    }

    fn restore(&mut self) {
        #[cfg(unix)]
        unsafe {
            if self.original_pgid > 0 {
                let _ = libc::tcsetpgrp(self.signal_fd, self.original_pgid);
            }
            let _ = libc::signal(libc::SIGTTOU, self.signal_handler);
            self.original_pgid = -1;
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

impl Drop for ResumeGuard {
    fn drop(&mut self) {
        self.resume_now();
    }
}

pub(crate) struct HqContext {
    pub(crate) supervisor: Option<std::sync::Arc<dyn SupervisorAgent>>,
    pub(crate) executor: Option<std::sync::Arc<dyn ExecutorAgent>>,
    /// Headless agent handles — executor and reviewer both run without a PTY.
    pub(crate) headless: std::collections::HashMap<String, agent_manager::HeadlessHandle>,
    pub(crate) last_task_state: Option<TaskState>,
    debug: bool,
    state_rx: watch::Receiver<Option<WatchedState>>,
    pub(crate) display: Display,
}

impl HqContext {
    fn new(state_rx: watch::Receiver<Option<WatchedState>>, display: Display, debug: bool) -> Self {
        Self {
            supervisor: None,
            executor: None,
            headless: std::collections::HashMap::new(),
            last_task_state: None,
            debug,
            state_rx,
            display,
        }
    }

    fn set_hq_config(&mut self, hq: &HqConfig) {
        self.supervisor = Some(std::sync::Arc::clone(&hq.supervisor));
        self.executor = Some(std::sync::Arc::clone(&hq.executor));
    }

    fn executor_agent_id(&self) -> Result<String> {
        let executor = self
            .executor
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Executor agent is not configured"))?;
        Ok(agent_id(
            ROLE_EXECUTOR,
            executor.name(),
            DEFAULT_AGENT_INDEX,
        ))
    }

    fn supervisor_agent_id(&self) -> Result<String> {
        let supervisor = self
            .supervisor
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Supervisor agent is not configured"))?;
        Ok(agent_id(
            ROLE_SUPERVISOR,
            supervisor.name(),
            DEFAULT_AGENT_INDEX,
        ))
    }

    async fn ensure_hq_config(&mut self) -> Result<()> {
        if self.supervisor.is_some() && self.executor.is_some() {
            return Ok(());
        }

        let config = Config::load().await?;
        let hq = config.hq.ok_or_else(|| {
            anyhow::anyhow!(
                "No [hq] section in ferrus.toml. Add:\n[hq]\nsupervisor = \"claude-code\"\nexecutor = \"codex\""
            )
        })?;
        self.set_hq_config(&hq);
        Ok(())
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

        match action {
            TransitionAction::SpawnExecutor => {
                if let Err(err) = self.ensure_hq_config().await {
                    self.display
                        .error(format!("Failed to load executor config: {err}"));
                    return;
                }
                let executor_id = match self.executor_agent_id() {
                    Ok(id) => id,
                    Err(err) => {
                        self.display.error(err.to_string());
                        return;
                    }
                };
                if let Err(err) = self
                    .spawn_headless_executor(&executor_id, agent_manager::executor_prompt())
                    .await
                {
                    self.display
                        .error(format!("Failed to spawn executor: {err}"));
                }
            }
            TransitionAction::SpawnReviewer => {
                // Executor submitted — terminate it so it doesn't compete with the next cycle.
                // Without the kill, the executor process stays alive in its wait_for_task loop.
                // If the supervisor later rejects, a new executor is spawned with the same
                // agent_id, and both race to claim the Addressing task via the idempotent
                // claim path — causing two executors to work concurrently.
                let executor_id = match self.executor_agent_id() {
                    Ok(id) => id,
                    Err(err) => {
                        self.display.error(err.to_string());
                        return;
                    }
                };
                self.shutdown_headless(&executor_id).await;
                if let Err(err) = self.ensure_hq_config().await {
                    self.display
                        .error(format!("Failed to load supervisor config: {err}"));
                    return;
                }
                let supervisor_id = match self.supervisor_agent_id() {
                    Ok(id) => id,
                    Err(err) => {
                        self.display.error(err.to_string());
                        return;
                    }
                };
                if let Err(err) = self
                    .spawn_headless_supervisor(&supervisor_id, agent_manager::reviewer_prompt())
                    .await
                {
                    self.display
                        .error(format!("Failed to spawn reviewer: {err}"));
                }
            }
            TransitionAction::SpawnConsultant => {
                if let Err(err) = self.ensure_hq_config().await {
                    self.display
                        .error(format!("Failed to load supervisor config: {err}"));
                    return;
                }
                let supervisor_id = match self.supervisor_agent_id() {
                    Ok(id) => id,
                    Err(err) => {
                        self.display.error(err.to_string());
                        return;
                    }
                };
                if let Err(err) = self
                    .spawn_headless_supervisor(&supervisor_id, agent_manager::consultant_prompt())
                    .await
                {
                    self.display
                        .error(format!("Failed to spawn consultation supervisor: {err}"));
                }
            }
            TransitionAction::KillReviewerSpawnExecutor => {
                let supervisor_id = match self.supervisor_agent_id() {
                    Ok(id) => id,
                    Err(err) => {
                        self.display.error(err.to_string());
                        return;
                    }
                };
                self.shutdown_headless(&supervisor_id).await;
                if let Err(err) = self.ensure_hq_config().await {
                    self.display
                        .error(format!("Failed to load executor config: {err}"));
                    return;
                }
                let executor_id = match self.executor_agent_id() {
                    Ok(id) => id,
                    Err(err) => {
                        self.display.error(err.to_string());
                        return;
                    }
                };
                if let Err(err) = self
                    .spawn_headless_executor(&executor_id, agent_manager::executor_prompt())
                    .await
                {
                    self.display
                        .error(format!("Failed to spawn executor: {err}"));
                }
            }
            TransitionAction::TaskComplete => {
                let agent_ids = [
                    self.executor_agent_id().ok(),
                    self.supervisor_agent_id().ok(),
                ];
                for name in agent_ids.into_iter().flatten() {
                    self.shutdown_headless(&name).await;
                }
                self.display
                    .info("Task complete! Use /plan to start a new task.");
            }
            TransitionAction::TaskFailed => {
                let agent_ids = [
                    self.executor_agent_id().ok(),
                    self.supervisor_agent_id().ok(),
                ];
                for name in agent_ids.into_iter().flatten() {
                    self.shutdown_headless(&name).await;
                }
                self.display
                    .info("Task failed. Use /status for details, /reset to try again.");
            }
            TransitionAction::PauseForHuman => match store::read_question().await {
                Ok(q) if !q.trim().is_empty() => {
                    self.display.info(format!(
                        "\n[AWAITING YOUR ANSWER]\n{q}\n\nType your answer and press Enter."
                    ));
                }
                _ => {
                    self.display
                        .info("[AWAITING YOUR ANSWER] Type your response and press Enter.");
                }
            },
            // (AwaitingHuman, Executing|Addressing|...) → NoOp: the executor either
            // resumed via /wait_for_answer (alive path) or was relaunched by answer()
            // (dead path). No further action needed from the state watcher.
            TransitionAction::NoOp => {}
        }
    }

    async fn spawn_headless_supervisor(&mut self, name: &str, prompt: &str) -> Result<()> {
        let existing_is_alive = self
            .headless
            .get(name)
            .map(agent_manager::HeadlessHandle::is_alive);
        if existing_is_alive == Some(true) {
            self.display
                .info(format!("{name} already running headlessly."));
            return Ok(());
        }
        if existing_is_alive == Some(false) {
            self.reap_headless(name).await;
        }

        let agent = std::sync::Arc::clone(
            self.supervisor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Supervisor agent is not configured"))?,
        );
        self.display
            .info(format!("Spawning {name} ({}) headlessly…", agent.name()));
        let handle =
            agent_manager::spawn_headless_supervisor(agent.as_ref(), name, prompt, self.debug)
                .await?;
        self.display.info(format!(
            "{name} started in background. Logs: {}",
            handle.log_path.display()
        ));
        self.headless.insert(name.to_string(), handle);
        Ok(())
    }

    async fn spawn_headless_executor(&mut self, name: &str, prompt: &str) -> Result<()> {
        let existing_is_alive = self
            .headless
            .get(name)
            .map(agent_manager::HeadlessHandle::is_alive);
        if existing_is_alive == Some(true) {
            self.display
                .info(format!("{name} already running headlessly."));
            return Ok(());
        }
        if existing_is_alive == Some(false) {
            self.reap_headless(name).await;
        }

        let agent = std::sync::Arc::clone(
            self.executor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Executor agent is not configured"))?,
        );
        self.display
            .info(format!("Spawning {name} ({}) headlessly…", agent.name()));
        let handle =
            agent_manager::spawn_headless_executor(agent.as_ref(), name, prompt, self.debug)
                .await?;
        self.display.info(format!(
            "{name} started in background. Logs: {}",
            handle.log_path.display()
        ));
        self.headless.insert(name.to_string(), handle);
        Ok(())
    }

    async fn resume(&mut self) -> Result<()> {
        if self
            .headless
            .iter()
            .any(|(name, handle)| name.starts_with(ROLE_EXECUTOR) && handle.is_alive())
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
            TaskState::Consultation => {
                self.ensure_hq_config().await?;
                let supervisor_id = self.supervisor_agent_id()?;
                self.shutdown_headless(&supervisor_id).await;
                self.spawn_headless_supervisor(
                    &supervisor_id,
                    agent_manager::consultant_resume_prompt(),
                )
                .await?;

                let executor_id = self.executor_agent_id()?;
                return self
                    .spawn_headless_executor(
                        &executor_id,
                        agent_manager::executor_wait_for_consult_prompt(),
                    )
                    .await;
            }
            _ => {}
        }

        self.ensure_hq_config().await?;

        // Use resume prompt if state is AwaitingHuman (executor was relaunched after answer).
        let prompt = if state.state == TaskState::AwaitingHuman {
            agent_manager::executor_resume_prompt()
        } else {
            agent_manager::executor_prompt()
        };

        let executor_id = self.executor_agent_id()?;
        self.spawn_headless_executor(&executor_id, prompt).await
    }

    async fn review(&mut self) -> Result<()> {
        let state = store::read_state().await?;
        if state.state != TaskState::Reviewing {
            anyhow::bail!(
                "State is {:?} — /review requires Reviewing. Use /status.",
                state.state
            );
        }

        self.ensure_hq_config().await?;
        let supervisor_id = self.supervisor_agent_id()?;
        self.spawn_headless_supervisor(&supervisor_id, agent_manager::reviewer_prompt())
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

        self.shutdown_all_headless().await;

        let mut reg = agents::read_agents().await?;
        for entry in &mut reg.agents {
            entry.pid = None;
            entry.status = agents::AgentStatus::Suspended;
        }
        agents::write_agents(&reg).await?;

        store::clear_task().await?;
        store::clear_submission().await?;
        store::clear_answer().await?;
        store::clear_consult_request().await?;
        store::clear_consult_response().await?;
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

        self.shutdown_all_headless().await;

        let mut reg = agents::read_agents().await?;
        for entry in &mut reg.agents {
            entry.pid = None;
            entry.status = agents::AgentStatus::Suspended;
        }
        agents::write_agents(&reg).await?;

        self.display.info("All agent sessions stopped.");
        Ok(())
    }

    async fn spawn_interactive_supervisor(
        &mut self,
        name: &str,
        prompt: Option<&str>,
    ) -> Result<()> {
        use crate::state::agents::{read_agents, write_agents, AgentEntry, AgentStatus};
        use std::process::Stdio;
        use tokio::process::Command;

        let agent = std::sync::Arc::clone(
            self.supervisor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Supervisor agent is not configured"))?,
        );
        let mut std_cmd = agent.spawn(prompt);
        agent_manager::configure_interactive_command(&mut std_cmd);
        let mut cmd = Command::from(std_cmd);

        let ack_rx = self.display.suspend();
        let _ = ack_rx.await;
        let mut guard = ResumeGuard::new(self.display.clone());
        let program = cmd.as_std().get_program().to_string_lossy().into_owned();

        let mut child = cmd
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("Failed to spawn {program}"))?;
        let pid = child.id().unwrap_or_default();
        let mut terminal = TerminalGuard::for_child(pid);

        {
            let mut reg = read_agents().await?;
            reg.upsert(AgentEntry {
                role: ROLE_SUPERVISOR.to_string(),
                agent_type: agent.name().to_string(),
                name: name.to_string(),
                pid: Some(pid),
                status: AgentStatus::Running,
                started_at: Some(chrono::Utc::now()),
            });
            write_agents(&reg).await?;
        }

        let _ = child.wait().await;
        terminal.restore();
        cleanup_process_group(pid);
        guard.resume_now();

        {
            let mut reg = read_agents().await?;
            if let Some(e) = reg.by_name_mut(name) {
                e.pid = None;
                e.status = AgentStatus::Suspended;
            }
            write_agents(&reg).await?;
        }

        Ok(())
    }

    async fn spawn_interactive_executor(&mut self, name: &str, prompt: Option<&str>) -> Result<()> {
        use crate::state::agents::{read_agents, write_agents, AgentEntry, AgentStatus};
        use std::process::Stdio;
        use tokio::process::Command;

        let agent = std::sync::Arc::clone(
            self.executor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Executor agent is not configured"))?,
        );
        let mut std_cmd = agent.spawn(prompt);
        agent_manager::configure_interactive_command(&mut std_cmd);
        let mut cmd = Command::from(std_cmd);

        let ack_rx = self.display.suspend();
        let _ = ack_rx.await;
        let mut guard = ResumeGuard::new(self.display.clone());
        let program = cmd.as_std().get_program().to_string_lossy().into_owned();

        let mut child = cmd
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("Failed to spawn {program}"))?;
        let pid = child.id().unwrap_or_default();
        let mut terminal = TerminalGuard::for_child(pid);

        {
            let mut reg = read_agents().await?;
            reg.upsert(AgentEntry {
                role: ROLE_EXECUTOR.to_string(),
                agent_type: agent.name().to_string(),
                name: name.to_string(),
                pid: Some(pid),
                status: AgentStatus::Running,
                started_at: Some(chrono::Utc::now()),
            });
            write_agents(&reg).await?;
        }

        let _ = child.wait().await;
        terminal.restore();
        cleanup_process_group(pid);
        guard.resume_now();

        {
            let mut reg = read_agents().await?;
            if let Some(e) = reg.by_name_mut(name) {
                e.pid = None;
                e.status = AgentStatus::Suspended;
            }
            write_agents(&reg).await?;
        }

        Ok(())
    }

    async fn plan(&mut self) -> Result<()> {
        self.ensure_hq_config().await?;
        let agent = std::sync::Arc::clone(
            self.supervisor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Supervisor agent is not configured"))?,
        );

        self.display.info(format!(
            "Spawning supervisor ({}) for free-form planning…",
            agent.name()
        ));

        let supervisor_id = self.supervisor_agent_id()?;
        self.spawn_interactive_supervisor(
            &supervisor_id,
            Some(agent_manager::supervisor_plan_prompt()),
        )
        .await
    }

    async fn task(&mut self) -> Result<()> {
        use std::process::Stdio;
        use tokio::process::Command;

        self.ensure_hq_config().await?;

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
                    "State is {other:?} — /task requires Idle or Complete. Use /reset first if needed."
                );
            }
        }

        let supervisor = std::sync::Arc::clone(
            self.supervisor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Supervisor agent is not configured"))?,
        );

        self.display
            .info(format!("Spawning supervisor ({})…", supervisor.name()));
        self.display
            .info("Collaborate with the supervisor to define the task.");

        let mut std_cmd = supervisor.spawn(Some(agent_manager::supervisor_task_prompt()));
        agent_manager::configure_interactive_command(&mut std_cmd);
        let mut cmd = Command::from(std_cmd);

        let ack_rx = self.display.suspend();
        let _ = ack_rx.await;
        let mut resume_guard = ResumeGuard::new(self.display.clone());
        let program = cmd.as_std().get_program().to_string_lossy().into_owned();
        let mut child = cmd
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("Failed to spawn {program}"))?;
        let supervisor_pid = child.id().unwrap_or_default();
        let mut terminal = TerminalGuard::for_child(supervisor_pid);

        let supervisor_id = self.supervisor_agent_id()?;
        {
            use agents::{read_agents, write_agents, AgentEntry, AgentStatus};

            let mut reg = read_agents().await?;
            reg.upsert(AgentEntry {
                role: ROLE_SUPERVISOR.to_string(),
                agent_type: supervisor.name().to_string(),
                name: supervisor_id.clone(),
                pid: Some(supervisor_pid),
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
                            terminate_process_group(supervisor_pid);
                            let _ = child.wait().await;
                            break;
                        }
                    }
                }
            }
        }
        terminal.restore();
        cleanup_process_group(supervisor_pid);
        resume_guard.resume_now();

        {
            use agents::{read_agents, write_agents, AgentStatus};

            let mut reg = read_agents().await?;
            if let Some(entry) = reg.by_name_mut(&supervisor_id) {
                entry.pid = None;
                entry.status = AgentStatus::Suspended;
            }
            write_agents(&reg).await?;
        }

        let new_state = store::read_state().await?;
        if new_state.state == TaskState::Executing {
            let executor_id = self.executor_agent_id()?;
            self.spawn_headless_executor(&executor_id, agent_manager::executor_prompt())
                .await?;
            self.display
                .info("Executor running headlessly. State changes print automatically.");
        } else {
            self.display.info(format!(
                "No task created (state is {:?}). Re-run /task when ready.",
                new_state.state
            ));
        }
        Ok(())
    }

    async fn supervisor_interactive(&mut self) -> Result<()> {
        self.ensure_hq_config().await?;
        let agent = std::sync::Arc::clone(
            self.supervisor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Supervisor agent is not configured"))?,
        );

        self.display.info(format!(
            "Spawning supervisor ({}) interactively…",
            agent.name()
        ));

        let supervisor_id = self.supervisor_agent_id()?;
        self.spawn_interactive_supervisor(&supervisor_id, None)
            .await
    }

    async fn executor_interactive(&mut self) -> Result<()> {
        self.ensure_hq_config().await?;
        let agent = std::sync::Arc::clone(
            self.executor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Executor agent is not configured"))?,
        );

        self.display.info(format!(
            "Spawning executor ({}) interactively…",
            agent.name()
        ));

        let executor_id = self.executor_agent_id()?;
        self.spawn_interactive_executor(&executor_id, None).await
    }

    /// Handle a raw-text answer from the user when state is AwaitingHuman.
    async fn answer(&mut self, response: String) -> Result<()> {
        if response.trim().is_empty() {
            anyhow::bail!("Answer cannot be empty.");
        }
        let state = store::read_state().await?;
        if state.state != TaskState::AwaitingHuman {
            anyhow::bail!(
                "State is {:?} — not currently waiting for an answer.",
                state.state
            );
        }

        // Write ANSWER.md. If the agent is alive and blocking on /wait_for_answer,
        // the tool will detect this, restore state, and return the answer automatically.
        store::write_answer(&response).await?;
        self.display
            .info("Answer recorded. Waiting for agent to resume…");

        // Fallback: if the agent has exited, /wait_for_answer will never poll again.
        // Restore state directly so the user can relaunch the agent manually.
        let agent_alive = self
            .executor_agent_id()
            .ok()
            .and_then(|id| self.headless.get(&id).map(|h| h.is_alive()))
            .unwrap_or(false)
            || self
                .supervisor_agent_id()
                .ok()
                .and_then(|id| self.headless.get(&id).map(|h| h.is_alive()))
                .unwrap_or(false);

        if !agent_alive {
            let mut st = store::read_state().await?;
            if st.state == TaskState::AwaitingHuman {
                let resumed = st.answer()?;
                store::write_state(&st).await?;
                store::clear_question().await?;
                let relaunch_hint = if resumed == TaskState::Reviewing {
                    "Use /review to relaunch the reviewer."
                } else {
                    "Use /resume to relaunch — it will read ANSWER.md and continue."
                };
                self.display.info(format!(
                    "Agent is not running. State restored to {resumed:?}. {relaunch_hint}"
                ));
            }
        }
        Ok(())
    }

    async fn shutdown_headless(&mut self, name: &str) {
        if let Some(handle) = self.headless.remove(name) {
            handle.terminate().await;
        }
    }

    async fn reap_headless(&mut self, name: &str) {
        if let Some(handle) = self.headless.remove(name) {
            handle.reap().await;
        }
    }

    async fn shutdown_all_headless(&mut self) {
        let handles: Vec<_> = self.headless.drain().map(|(_, handle)| handle).collect();
        for handle in handles {
            handle.terminate().await;
        }
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

fn terminate_process_group(pid: u32) {
    if pid == 0 {
        return;
    }
    agent_manager::signal_process(pid, libc::SIGTERM);
    std::thread::sleep(std::time::Duration::from_millis(250));
    agent_manager::signal_process(pid, libc::SIGKILL);
}

fn cleanup_process_group(pid: u32) {
    if pid == 0 {
        return;
    }
    agent_manager::signal_process(pid, libc::SIGTERM);
}

#[allow(dead_code)]
#[derive(Debug, PartialEq)]
pub(crate) enum TransitionAction {
    SpawnExecutor,
    SpawnReviewer,
    SpawnConsultant,
    KillReviewerSpawnExecutor,
    TaskComplete,
    TaskFailed,
    /// Executor asked a question; display it and wait for user input.
    PauseForHuman,
    NoOp,
}

#[allow(dead_code)]
pub(crate) fn transition_action(from: &TaskState, to: &TaskState) -> TransitionAction {
    use TaskState::*;

    match (from, to) {
        (Idle, Executing) => TransitionAction::SpawnExecutor,
        (Executing | Addressing | Checking, Reviewing) => TransitionAction::SpawnReviewer,
        (Executing | Addressing | Checking, Consultation) => TransitionAction::SpawnConsultant,
        (Reviewing, Addressing) => TransitionAction::KillReviewerSpawnExecutor,
        (Reviewing, Complete) => TransitionAction::TaskComplete,
        (_, Failed) => TransitionAction::TaskFailed,
        (Consultation, Executing | Addressing | Checking) => TransitionAction::NoOp,
        // Executor paused to ask the human a question.
        (Executing | Addressing | Checking | Reviewing, AwaitingHuman) => {
            TransitionAction::PauseForHuman
        }
        // State restored after human answered:
        //   - alive path: /wait_for_answer unblocked the still-running executor
        //   - dead path: answer() in HqContext restored state; user will /execute
        // Either way, no spawning needed here.
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
    fn executing_to_consultation_spawns_consultant() {
        assert_eq!(
            transition_action(&Executing, &Consultation),
            TransitionAction::SpawnConsultant
        );
    }

    #[test]
    fn consultation_to_executing_is_noop() {
        assert_eq!(
            transition_action(&Consultation, &Executing),
            TransitionAction::NoOp
        );
    }

    #[test]
    fn stale_pid_detection() {
        assert!(pid_is_alive(std::process::id()));
        assert!(!pid_is_alive(999999));
    }

    #[test]
    fn executing_to_awaiting_human_pauses() {
        assert_eq!(
            transition_action(&Executing, &AwaitingHuman),
            TransitionAction::PauseForHuman
        );
    }

    #[test]
    fn addressing_to_awaiting_human_pauses() {
        assert_eq!(
            transition_action(&Addressing, &AwaitingHuman),
            TransitionAction::PauseForHuman
        );
    }

    #[test]
    fn reviewing_to_awaiting_human_pauses() {
        assert_eq!(
            transition_action(&Reviewing, &AwaitingHuman),
            TransitionAction::PauseForHuman
        );
    }

    #[test]
    fn awaiting_human_to_executing_is_noop() {
        // Executor resumes via /wait_for_answer (alive) or is relaunched by answer()
        // (dead). Either way, HQ doesn't spawn again from this transition.
        assert_eq!(
            transition_action(&AwaitingHuman, &Executing),
            TransitionAction::NoOp
        );
    }
}
