pub mod agent_manager;
mod commands;
mod display;
mod state_watcher;
mod tui;

use anyhow::{Context, Result};
use tokio::process::Command;
use tokio::sync::watch;

use crate::agent_id::{DEFAULT_AGENT_INDEX, ROLE_EXECUTOR, ROLE_SUPERVISOR, agent_id};
use crate::agents::{AgentRunMode, ExecutorAgent, SupervisorAgent};
use crate::checks::runner;
use crate::config::{Config, HqConfig, HqRole, update_hq_agent_config};
use crate::platform;
use crate::state::{
    agents,
    machine::{StateData, TaskState},
    store,
};
use crate::update_check;
use commands::{ModelTarget, ShellCommand, parse_command};
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

    let update_display = display.clone();
    tokio::spawn(async move {
        if let Some(message) = update_check::notification_message().await {
            update_display.tip(message);
        }
    });

    let mut tui_task = tokio::spawn(tui::run_tui(
        msg_rx,
        cmd_tx,
        state_rx.clone(),
        debug,
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
                        let line = cmd.as_str();
                        if line.trim().is_empty() {
                            continue;
                        }
                        if line.trim() == "/quit" {
                            ctx.display.muted("Bye.");
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
    let supervisor = match hq.supervisor_agent() {
        Ok(agent) => {
            load_agent_version_from_command(agent.spawn(AgentRunMode::Interactive { prompt: None }))
                .await
        }
        Err(_) => String::new(),
    };
    let executor = match hq.executor_agent() {
        Ok(agent) => {
            load_agent_version_from_command(agent.spawn(AgentRunMode::Interactive { prompt: None }))
                .await
        }
        Err(_) => String::new(),
    };
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
            ctx.display.muted("Bye.");
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
        ShellCommand::Check { force } => ctx.check(force).await?,
        ShellCommand::Help => {
            ctx.display.info(concat!(
                "ferrus HQ commands:\n",
                "  /plan              Free-form planning session with the supervisor\n",
                "  /task              Define a task with the supervisor, then run executor→review loop\n",
                "  /check             Run the Ferrus check gate deterministically from HQ\n",
                "  /check --force     Run configured checks from HQ without state requirements\n",
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
                "  /model <role> <model>\n",
                "                     Update the configured model override\n",
                "  /model <role> --clear\n",
                "                     Clear the configured model override\n",
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
            supervisor_model,
            executor,
            executor_model,
        } => {
            let sup = supervisor.as_deref().and_then(parse_agent_type);
            let exe = executor.as_deref().and_then(parse_agent_type);
            if sup.is_none() && exe.is_none() {
                ctx.display
                    .error("At least one of --supervisor or --executor required");
            } else {
                crate::cli::commands::register::run(sup, supervisor_model, exe, executor_model)
                    .await?;
                ctx.reload_hq_config().await?;
            }
        }
        ShellCommand::Model {
            target,
            model,
            clear,
        } => {
            let model = match (model, clear) {
                (Some(model), false) => Some(model),
                (None, true) => None,
                _ => anyhow::bail!(
                    "Usage: /model <supervisor|executor> <model> | /model <supervisor|executor> --clear"
                ),
            };
            ctx.update_model(target, model.as_deref()).await?;
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

impl Drop for ResumeGuard {
    fn drop(&mut self) {
        self.resume_now();
    }
}

impl ModelTarget {
    fn config_role(self) -> HqRole {
        match self {
            Self::Supervisor => HqRole::Supervisor,
            Self::Executor => HqRole::Executor,
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::Supervisor => "Supervisor",
            Self::Executor => "Executor",
        }
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
        self.supervisor = hq.supervisor_agent().ok();
        self.executor = hq.executor_agent().ok();
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
                "No [hq.supervisor] / [hq.executor] sections in ferrus.toml. Add:\n[hq.supervisor]\nagent = \"claude-code\"\nmodel = \"\"\n\n[hq.executor]\nagent = \"codex\"\nmodel = \"\""
            )
        })?;
        self.set_hq_config(&hq);
        Ok(())
    }

    async fn reload_hq_config(&mut self) -> Result<()> {
        let config = Config::load().await?;
        let hq = config.hq.ok_or_else(|| {
            anyhow::anyhow!("No [hq.supervisor] / [hq.executor] sections in ferrus.toml.")
        })?;
        self.set_hq_config(&hq);
        Ok(())
    }

    async fn update_model(&mut self, target: ModelTarget, model: Option<&str>) -> Result<()> {
        self.ensure_hq_config().await?;
        update_hq_agent_config(target.config_role(), None, Some(model)).await?;
        self.reload_hq_config().await?;
        if let Some(model) = model {
            self.display.info(format!(
                "{} model set to \"{model}\"",
                target.display_name()
            ));
        } else {
            self.display
                .info(format!("{} model cleared", target.display_name()));
        }
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

        let result = match action {
            TransitionAction::SpawnExecutor => self.handle_spawn_executor_transition().await,
            TransitionAction::SpawnReviewer => self.handle_spawn_reviewer_transition().await,
            TransitionAction::SpawnConsultant => self.handle_spawn_consultant_transition().await,
            TransitionAction::KillReviewerSpawnExecutor => {
                self.handle_restart_executor_transition().await
            }
            TransitionAction::TaskComplete => {
                self.handle_terminal_tip("Tip: Use /task to start a new task.")
                    .await
            }
            TransitionAction::TaskFailed => {
                self.handle_terminal_tip("Tip: Use /status for details, /reset to try again.")
                    .await
            }
            TransitionAction::PauseForHuman => self.handle_pause_for_human().await,
            // (AwaitingHuman, Executing|Addressing|...) → NoOp: the executor either
            // resumed via /wait_for_answer (alive path) or was relaunched by answer()
            // (dead path). No further action needed from the state watcher.
            TransitionAction::NoOp => Ok(()),
        };

        if let Err(err) = result {
            self.display.error(err.to_string());
        }
    }

    async fn handle_spawn_executor_transition(&mut self) -> Result<()> {
        let executor_id = self
            .executor_agent_id_after_config()
            .await
            .context("Failed to load executor config")?;
        self.spawn_headless_executor(&executor_id, agent_manager::executor_prompt())
            .await
            .context("Failed to spawn executor")
    }

    async fn handle_spawn_reviewer_transition(&mut self) -> Result<()> {
        let executor_id = self.executor_agent_id()?;
        self.shutdown_headless(&executor_id).await;
        let supervisor_id = self
            .supervisor_agent_id_after_config()
            .await
            .context("Failed to load supervisor config")?;
        self.spawn_headless_supervisor(&supervisor_id, agent_manager::reviewer_prompt())
            .await
            .context("Failed to spawn reviewer")
    }

    async fn handle_spawn_consultant_transition(&mut self) -> Result<()> {
        let supervisor_id = self
            .supervisor_agent_id_after_config()
            .await
            .context("Failed to load supervisor config")?;
        self.spawn_headless_supervisor(&supervisor_id, agent_manager::consultant_prompt())
            .await
            .context("Failed to spawn consultation supervisor")
    }

    async fn handle_restart_executor_transition(&mut self) -> Result<()> {
        let supervisor_id = self.supervisor_agent_id()?;
        self.shutdown_headless(&supervisor_id).await;
        let executor_id = self
            .executor_agent_id_after_config()
            .await
            .context("Failed to load executor config")?;
        self.spawn_headless_executor(&executor_id, agent_manager::executor_prompt())
            .await
            .context("Failed to spawn executor")
    }

    async fn handle_terminal_tip(&mut self, message: &str) -> Result<()> {
        let agent_ids = [
            self.executor_agent_id().ok(),
            self.supervisor_agent_id().ok(),
        ];
        for name in agent_ids.into_iter().flatten() {
            self.shutdown_headless(&name).await;
        }
        self.display.tip(message);
        Ok(())
    }

    async fn handle_pause_for_human(&mut self) -> Result<()> {
        match store::read_question().await {
            Ok(q) if !q.trim().is_empty() => {
                self.display.info(format!(
                    "\n[AWAITING YOUR ANSWER]\n{q}\n\nType your answer and press Enter."
                ));
            }
            _ => {
                self.display
                    .info("[AWAITING YOUR ANSWER] Type your response and press Enter.");
            }
        }
        Ok(())
    }

    async fn executor_agent_id_after_config(&mut self) -> Result<String> {
        self.ensure_hq_config().await?;
        self.executor_agent_id()
    }

    async fn supervisor_agent_id_after_config(&mut self) -> Result<String> {
        self.ensure_hq_config().await?;
        self.supervisor_agent_id()
    }

    async fn mark_agent_running(
        &self,
        role: &str,
        agent_type: &str,
        name: &str,
        pid: Option<u32>,
    ) -> Result<()> {
        use agents::{AgentEntry, AgentStatus, read_agents, write_agents};

        let mut reg = read_agents().await?;
        reg.upsert(AgentEntry {
            role: role.to_string(),
            agent_type: agent_type.to_string(),
            name: name.to_string(),
            pid,
            status: AgentStatus::Running,
            started_at: Some(chrono::Utc::now()),
        });
        write_agents(&reg).await
    }

    async fn mark_agent_suspended(&self, name: &str) -> Result<()> {
        use agents::{AgentStatus, read_agents, write_agents};

        let mut reg = read_agents().await?;
        if let Some(entry) = reg.by_name_mut(name) {
            entry.pid = None;
            entry.status = AgentStatus::Suspended;
        }
        write_agents(&reg).await
    }

    async fn spawn_interactive_command(
        &mut self,
        role: &str,
        agent_type: &str,
        name: &str,
        command: std::process::Command,
    ) -> Result<()> {
        use std::process::Stdio;
        use tokio::process::Command;

        let mut cmd = Command::from(command);
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
        self.mark_agent_running(role, agent_type, name, child.id())
            .await?;

        let _ = child.wait().await;
        guard.resume_now();
        self.mark_agent_suspended(name).await?;
        Ok(())
    }

    async fn prepare_headless_slot(&mut self, name: &str) -> bool {
        let existing_is_alive = self
            .headless
            .get(name)
            .map(agent_manager::HeadlessHandle::is_alive);
        if existing_is_alive == Some(true) {
            self.display.info(format!("{name} is already running."));
            return false;
        }
        if existing_is_alive == Some(false) {
            self.reap_headless(name).await;
        }
        true
    }

    fn store_headless_handle(&mut self, name: &str, handle: agent_manager::HeadlessHandle) {
        self.display.muted(format!(
            "  • Spawning {name}…\n  ╰─ Logs: {}\n\n",
            handle.log_path.display()
        ));
        self.headless.insert(name.to_string(), handle);
    }

    async fn spawn_headless_supervisor(&mut self, name: &str, prompt: &str) -> Result<()> {
        if !self.prepare_headless_slot(name).await {
            return Ok(());
        }

        let agent = std::sync::Arc::clone(
            self.supervisor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Supervisor agent is not configured"))?,
        );
        let handle =
            agent_manager::spawn_headless_supervisor(agent.as_ref(), name, prompt, self.debug)
                .await?;
        self.store_headless_handle(name, handle);
        Ok(())
    }

    async fn spawn_headless_executor(&mut self, name: &str, prompt: &str) -> Result<()> {
        if !self.prepare_headless_slot(name).await {
            return Ok(());
        }

        let agent = std::sync::Arc::clone(
            self.executor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Executor agent is not configured"))?,
        );
        let handle =
            agent_manager::spawn_headless_executor(agent.as_ref(), name, prompt, self.debug)
                .await?;
        self.store_headless_handle(name, handle);
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
                    .info("Task is already complete. Use /task to start a new task.");
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

    async fn check(&mut self, force: bool) -> Result<()> {
        if force {
            let config = Config::load().await?;
            if config.checks.commands.is_empty() {
                self.display.info(
                    "Checks passed. Warning: no check commands are configured in ferrus.toml.",
                );
                return Ok(());
            }

            let result = runner::run_checks(&config.checks.commands).await?;
            if result.passed {
                self.display
                    .info("All configured checks passed. State was not modified.");
            } else {
                let failed = result
                    .commands
                    .iter()
                    .filter(|cmd| !cmd.passed)
                    .map(|cmd| format!("- `{}`", cmd.command))
                    .collect::<Vec<_>>()
                    .join("\n");
                self.display.error(format!(
                    "Forced HQ checks failed. State was not modified.\n\nFailed commands:\n{failed}"
                ));
            }
            return Ok(());
        }

        let result = crate::server::tools::check::handler()
            .await
            .map_err(|err| anyhow::anyhow!(err.to_string()))?;
        self.display.info(result);
        Ok(())
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
                self.display.muted("Reset cancelled.");
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
            self.display.muted("Stop cancelled.");
            return Ok(());
        }

        self.shutdown_all_headless().await;

        let mut reg = agents::read_agents().await?;
        for entry in &mut reg.agents {
            entry.pid = None;
            entry.status = agents::AgentStatus::Suspended;
        }
        agents::write_agents(&reg).await?;

        self.display.muted("All agent sessions stopped.");
        Ok(())
    }

    async fn spawn_interactive_supervisor(
        &mut self,
        name: &str,
        prompt: Option<&str>,
    ) -> Result<()> {
        let agent = std::sync::Arc::clone(
            self.supervisor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Supervisor agent is not configured"))?,
        );
        self.spawn_interactive_command(
            ROLE_SUPERVISOR,
            agent.name(),
            name,
            agent.spawn(AgentRunMode::Interactive { prompt }),
        )
        .await
    }

    async fn spawn_interactive_executor(&mut self, name: &str, prompt: Option<&str>) -> Result<()> {
        let agent = std::sync::Arc::clone(
            self.executor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Executor agent is not configured"))?,
        );
        self.spawn_interactive_command(
            ROLE_EXECUTOR,
            agent.name(),
            name,
            agent.spawn(AgentRunMode::Interactive { prompt }),
        )
        .await
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

        let mut cmd = Command::from(supervisor.spawn(AgentRunMode::Interactive {
            prompt: Some(agent_manager::supervisor_task_prompt()),
        }));

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
        let supervisor_id = self.supervisor_agent_id()?;
        self.mark_agent_running(
            ROLE_SUPERVISOR,
            supervisor.name(),
            &supervisor_id,
            child.id(),
        )
        .await?;

        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(300));
        loop {
            tokio::select! {
                _ = child.wait() => break,
                _ = ticker.tick() => {
                    if let Ok(s) = store::read_state().await
                        && s.state == TaskState::Executing {
                        self.display.muted("Task created — stopping supervisor…");
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        break;
                    }
                }
            }
        }
        resume_guard.resume_now();

        self.mark_agent_suspended(&supervisor_id).await?;

        let new_state = store::read_state().await?;
        if new_state.state == TaskState::Executing {
            // Let the state watcher handle Idle -> Executing consistently.
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

async fn reconcile_agent_pids() {
    use crate::state::agents::{AgentStatus, read_agents, write_agents};

    if let Ok(mut reg) = read_agents().await {
        let mut changed = false;
        for entry in &mut reg.agents {
            if entry.status == AgentStatus::Running {
                let alive = entry.pid.map(platform::pid_is_alive).unwrap_or(false);
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
        (Executing | Addressing, Reviewing) => TransitionAction::SpawnReviewer,
        (Executing | Addressing, Consultation) => TransitionAction::SpawnConsultant,
        (Reviewing, Addressing) => TransitionAction::KillReviewerSpawnExecutor,
        (Reviewing, Complete) => TransitionAction::TaskComplete,
        (_, Failed) => TransitionAction::TaskFailed,
        (Consultation, Executing | Addressing) => TransitionAction::NoOp,
        // Executor paused to ask the human a question.
        (Executing | Addressing | Reviewing, AwaitingHuman) => TransitionAction::PauseForHuman,
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
    fn executing_to_addressing_is_noop() {
        assert_eq!(
            transition_action(&Executing, &Addressing),
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
        assert!(platform::pid_is_alive(std::process::id()));
        assert!(!platform::pid_is_alive(999999));
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
    fn fixing_to_awaiting_human_pauses() {
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
