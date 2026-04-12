use anyhow::{Context, Result};
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command as StdCommand, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::process::Command;

use crate::agent_id::{ROLE_EXECUTOR, ROLE_SUPERVISOR};
use crate::agents::{ExecutorAgent, SupervisorAgent};
use crate::state::agents::{read_agents, write_agents, AgentEntry, AgentStatus};

const SUPERVISOR_TASK_PROMPT: &str = "You are a Ferrus Supervisor in TASK DEFINITION mode.

Your goal: define a clear, executable task.

Do:
  - understand the user request
  - clarify if needed
  - draft a precise task
  - show the draft task text to the user
  - revise it if needed
  - get explicit user approval before creating the task

HARD RULES:
  - do NOT implement code
  - do NOT edit files
  - do NOT perform the task yourself
  - do NOT call /create_task until the user has explicitly approved the task text
  - the text you pass to /create_task should match the approved draft closely

After explicit approval: call /create_task, then stop.

Follow ROLE.md and SKILL.md.
";

const SUPERVISOR_PLAN_PROMPT: &str = "You are a Ferrus Supervisor in free-form planning mode.

Your goal: explore ideas, clarify problems, and help design solutions.

Stay at planning level unless explicitly asked to implement.

Follow ROLE.md and SKILL.md.
";

const REVIEWER_PROMPT: &str = "You are a Ferrus Supervisor in REVIEW mode.

Your goal: evaluate the submission and decide:

  - /approve
  - /reject with actionable feedback

HARD RULES:
  - Do NOT implement fixes
  - Do NOT modify `.ferrus/` or repository files to force progress
  - Read the submission via Ferrus review tools, then decide and exit

Follow ROLE.md and SKILL.md.
";

const EXECUTOR_PROMPT: &str = "You are a Ferrus Executor.

Your goal: take assigned work through implementation, /check, and /submit.

Required workflow:
  - call /wait_for_task as the first action in this session
  - implement the task
  - verify via /check
  - when /check passes, immediately call /submit
  - after /submit, stop; if review is rejected, HQ will start a fresh Executor session

Critical rules:
  - NEVER run tests/builds manually — always use /check
  - A green /check is not completion by itself; /submit is required
  - Use /consult for technical uncertainty
  - Use /ask_human only when information is missing or a decision is required
  - Do NOT emulate Ferrus tools by editing `.ferrus/` files or manually advancing state
  - If a Ferrus MCP tool is cancelled or fails, retry that tool; do NOT reconstruct task state from `.ferrus/`
  - You run headlessly — do not ask questions in the terminal

Follow ROLE.md and SKILL.md for full behavior.
";

const EXECUTOR_RESUME_PROMPT: &str = "You are a Ferrus Executor resuming work.

The human answer is in .ferrus/ANSWER.md.

Next steps:
  - read the answer
  - continue the task from where you left off
  - keep using Ferrus MCP tools for state transitions; do NOT emulate them via `.ferrus/`

Critical rules:
  - NEVER run tests/builds manually — always use /check
  - When /check passes, your next action must be /submit

Follow ROLE.md and SKILL.md.
";

const EXECUTOR_WAIT_FOR_CONSULT_PROMPT: &str =
    "You are a Ferrus Executor waiting for a supervisor consultation.

CONSULT_REQUEST.md already exists.

Your next step:
  - Call /wait_for_consult

Rules:
  - Do not perform any implementation until consultation is resolved
  - Follow standard Executor rules after receiving the answer
  - Do not try to recover consultation manually from `.ferrus/`; wait for the tool response
";

const CONSULTANT_PROMPT: &str = "
You are a Ferrus Supervisor in CONSULTATION mode.

Your goal: resolve the Executor's uncertainty with a clear, actionable answer.

Workflow:
  - read TASK.md and CONSULT_REQUEST.md
  - inspect relevant code if needed
  - provide a concrete answer via /respond_consult

Rules:
  - DO NOT implement code
  - DO NOT modify files
  - DO NOT call /create_task, /approve, /reject, or /submit

Focus:
  - eliminate ambiguity
  - give direct guidance (what to do, not just why)

After /respond_consult, exit.

Follow ROLE.md and SKILL.md.
";

const CONSULTANT_RESUME_PROMPT: &str = "
You are a Ferrus Supervisor resuming a consultation.

CONSULT_REQUEST.md already exists.

Workflow:
  - Read the request
  - Investigate the repository
  - Provide a clear answer via /respond_consult

Follow all standard CONSULTANT rules.

Exit immediately after responding.
";

#[allow(dead_code)]
/// Spawn an executor with inherited stdio and wait for it to exit.
/// Returns the exit code.
pub async fn spawn_and_wait_executor(
    agent: &dyn ExecutorAgent,
    name: &str,
    prompt: Option<&str>,
) -> Result<i32> {
    spawn_and_wait(agent.name(), agent.spawn(prompt), ROLE_EXECUTOR, name).await
}

#[allow(dead_code)]
/// Spawn a supervisor with inherited stdio and wait for it to exit.
/// Returns the exit code.
pub async fn spawn_and_wait_supervisor(
    agent: &dyn SupervisorAgent,
    name: &str,
    prompt: Option<&str>,
) -> Result<i32> {
    spawn_and_wait(agent.name(), agent.spawn(prompt), ROLE_SUPERVISOR, name).await
}

async fn spawn_and_wait(
    agent_type: &str,
    command: std::process::Command,
    role: &str,
    name: &str,
) -> Result<i32> {
    let mut cmd = Command::from(command);
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let mut reg = read_agents().await?;
    reg.upsert(AgentEntry {
        role: role.to_string(),
        agent_type: agent_type.to_string(),
        name: name.to_string(),
        pid: None,
        status: AgentStatus::Running,
        started_at: Some(chrono::Utc::now()),
    });
    write_agents(&reg).await?;

    let program = cmd.as_std().get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to spawn {program} as {role}"))?;

    let pid = child.id();
    let mut reg = read_agents().await?;
    if let Some(e) = reg.by_name_mut(name) {
        e.pid = pid;
    }
    write_agents(&reg).await?;

    let status = child
        .wait()
        .await
        .with_context(|| format!("{program} process error"))?;

    let mut reg = read_agents().await?;
    if let Some(e) = reg.by_name_mut(name) {
        e.pid = None;
        e.status = AgentStatus::Suspended;
    }
    write_agents(&reg).await?;

    Ok(status.code().unwrap_or(-1))
}

#[allow(dead_code)]
/// Best-effort cleanup: send SIGTERM to a role's process and mark it Suspended.
///
/// In Phase A this is rarely needed — foreground workers exit naturally.
/// Use this only as an edge-case cleanup helper, not a primary control path.
/// Unix-only; no-op on other platforms.
pub async fn kill_role(role: &str) -> Result<()> {
    let mut reg = read_agents().await?;
    if let Some(e) = reg.by_role_mut(role) {
        if let Some(pid) = e.pid {
            #[cfg(unix)]
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
            e.pid = None;
            e.status = AgentStatus::Suspended;
        }
    }
    write_agents(&reg).await?;
    Ok(())
}

pub fn executor_prompt() -> &'static str {
    EXECUTOR_PROMPT
}
pub fn executor_resume_prompt() -> &'static str {
    EXECUTOR_RESUME_PROMPT
}
pub fn executor_wait_for_consult_prompt() -> &'static str {
    EXECUTOR_WAIT_FOR_CONSULT_PROMPT
}
pub fn consultant_prompt() -> &'static str {
    CONSULTANT_PROMPT
}
pub fn consultant_resume_prompt() -> &'static str {
    CONSULTANT_RESUME_PROMPT
}
pub fn reviewer_prompt() -> &'static str {
    REVIEWER_PROMPT
}
pub fn supervisor_plan_prompt() -> &'static str {
    SUPERVISOR_PLAN_PROMPT
}
pub fn supervisor_task_prompt() -> &'static str {
    SUPERVISOR_TASK_PROMPT
}

pub(crate) fn configure_headless_command(command: &mut StdCommand) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        // SAFETY: these libc calls are async-signal-safe and operate only on the
        // child process between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }

                #[cfg(target_os = "linux")]
                {
                    if libc::prctl(
                        libc::PR_SET_PDEATHSIG,
                        libc::SIGTERM as libc::c_ulong,
                        0 as libc::c_ulong,
                        0 as libc::c_ulong,
                        0 as libc::c_ulong,
                    ) != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                }

                Ok(())
            });
        }
    }
}

pub(crate) fn signal_process(pid: u32, signal: libc::c_int) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as libc::pid_t), signal);
    }

    #[cfg(not(unix))]
    {
        let _ = (pid, signal);
    }
}

/// Handle for a headless background executor process.
pub struct HeadlessHandle {
    #[allow(dead_code)]
    pub name: String,
    pub log_path: PathBuf,
    pub pid: u32,
    pub exit_rx: tokio::sync::watch::Receiver<Option<i32>>,
    wait_thread: Option<std::thread::JoinHandle<()>>,
    output_threads: Vec<std::thread::JoinHandle<()>>,
}

impl HeadlessHandle {
    pub fn is_alive(&self) -> bool {
        self.exit_rx.borrow().is_none()
    }

    pub async fn terminate(mut self) {
        let _ = tokio::task::spawn_blocking(move || self.blocking_shutdown(true)).await;
    }

    pub async fn reap(mut self) {
        let _ = tokio::task::spawn_blocking(move || self.blocking_shutdown(false)).await;
    }

    fn send_signal(&self, signal: libc::c_int) {
        signal_process(self.pid, signal);
    }

    fn blocking_shutdown(&mut self, terminate: bool) {
        if terminate && self.is_alive() {
            self.send_signal(libc::SIGTERM);
            std::thread::sleep(Duration::from_millis(250));
            if self.is_alive() {
                self.send_signal(libc::SIGKILL);
            }
        }

        if let Some(wait_thread) = self.wait_thread.take() {
            let _ = wait_thread.join();
        }
        for output_thread in self.output_threads.drain(..) {
            let _ = output_thread.join();
        }
    }
}

impl Drop for HeadlessHandle {
    fn drop(&mut self) {
        self.blocking_shutdown(true);
    }
}

pub async fn spawn_headless_executor(
    agent: &dyn ExecutorAgent,
    name: &str,
    prompt: &str,
    debug: bool,
) -> Result<HeadlessHandle> {
    spawn_headless(
        agent.name(),
        agent.spawn_headlessly(prompt),
        ROLE_EXECUTOR,
        name,
        prompt,
        debug,
    )
    .await
}

pub async fn spawn_headless_supervisor(
    agent: &dyn SupervisorAgent,
    name: &str,
    prompt: &str,
    debug: bool,
) -> Result<HeadlessHandle> {
    spawn_headless(
        agent.name(),
        agent.spawn_headlessly(prompt),
        ROLE_SUPERVISOR,
        name,
        prompt,
        debug,
    )
    .await
}

async fn spawn_headless(
    agent_type: &str,
    mut command: StdCommand,
    role: &str,
    name: &str,
    prompt: &str,
    debug: bool,
) -> Result<HeadlessHandle> {
    let log_dir = std::path::Path::new(".ferrus/logs");
    tokio::fs::create_dir_all(log_dir)
        .await
        .context("Failed to create .ferrus/logs")?;
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S");
    let log_path = log_dir.join(format!("{role}_{ts}.log"));

    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("Failed to open log file {}", log_path.display()))?;
    if debug {
        append_debug_agent_flags(agent_type, &mut command);
    }
    let command_summary = format_command(&command);

    let logger = if debug {
        let log_stderr = log_file
            .try_clone()
            .context("Failed to clone log file handle")?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_stderr));
        None
    } else {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Some(Arc::new(Mutex::new(SlimLogger::new(log_file))))
    };

    configure_headless_command(&mut command);

    let mut child = command.spawn().with_context(|| {
        format!(
            "Failed to spawn {} headlessly as {role}",
            command.get_program().to_string_lossy()
        )
    })?;

    let pid = child.id();
    let mut output_threads = Vec::new();

    if let Some(logger) = logger.as_ref() {
        let mut logger = logger.lock().expect("logger poisoned");
        logger.log_event(
            "Started",
            format!("{name} ({role}, {agent_type}, pid {pid})"),
        )?;
        logger.log_event("Agent meta", &command_summary)?;
        logger.log_initial_prompt(prompt)?;
    }

    if let Some(logger) = logger.as_ref() {
        if let Some(stdout) = child.stdout.take() {
            output_threads.push(spawn_slim_log_reader(stdout, Arc::clone(logger)));
        }
        if let Some(stderr) = child.stderr.take() {
            output_threads.push(spawn_slim_log_reader(stderr, Arc::clone(logger)));
        }
    }

    let mut reg = read_agents().await?;
    reg.upsert(AgentEntry {
        role: role.to_string(),
        agent_type: agent_type.to_string(),
        name: name.to_string(),
        pid: Some(pid),
        status: AgentStatus::Running,
        started_at: Some(chrono::Utc::now()),
    });
    write_agents(&reg).await?;

    let (exit_tx, exit_rx) = tokio::sync::watch::channel::<Option<i32>>(None);
    let wait_logger = logger.clone();
    let wait_thread = std::thread::spawn(move || {
        let code = child.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
        if let Some(logger) = wait_logger {
            let mut logger = logger.lock().expect("logger poisoned");
            logger.flush_pending_error();
            let _ = logger.log_event("Finished", format!("exit code {code}"));
        }
        let _ = exit_tx.send(Some(code));
    });
    let name_owned = name.to_string();
    let mut registry_exit_rx = exit_rx.clone();
    tokio::spawn(async move {
        if registry_exit_rx.changed().await.is_err() {
            return;
        }

        if let Ok(mut reg) = read_agents().await {
            if let Some(e) = reg.by_name_mut(&name_owned) {
                e.pid = None;
                e.status = AgentStatus::Suspended;
            }
            let _ = write_agents(&reg).await;
        }
    });

    Ok(HeadlessHandle {
        name: name.to_string(),
        log_path,
        pid,
        exit_rx,
        wait_thread: Some(wait_thread),
        output_threads,
    })
}

fn append_debug_agent_flags(agent_type: &str, command: &mut StdCommand) {
    match agent_type {
        "claude-code" => {
            command.arg("--verbose");
        }
        // `codex --help` and `codex exec --help` expose no verbose/debug flag.
        "codex" => {}
        _ => {}
    }
}

fn format_command(command: &StdCommand) -> String {
    let program = command.get_program().to_string_lossy();
    let args = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    format!("command={program} args={args:?}")
}

fn spawn_slim_log_reader<R>(
    reader: R,
    logger: Arc<Mutex<SlimLogger>>,
) -> std::thread::JoinHandle<()>
where
    R: std::io::Read + Send + 'static,
{
    std::thread::spawn(move || {
        let reader = BufReader::new(reader);
        for line in reader.lines() {
            let Ok(line) = line else {
                break;
            };
            if line.trim().is_empty() {
                continue;
            }
            let mut logger = logger.lock().expect("logger poisoned");
            let _ = logger.handle_agent_output(&line);
        }
    })
}

struct SlimLogger {
    file: File,
    pending_failed_tool: Option<String>,
}

impl SlimLogger {
    fn new(file: File) -> Self {
        Self {
            file,
            pending_failed_tool: None,
        }
    }

    fn handle_agent_output(&mut self, line: &str) -> std::io::Result<()> {
        if let Some((tool, status)) = parse_mcp_tool_status(line) {
            self.flush_pending_error();
            match status {
                ToolCallStatus::Started => {}
                ToolCallStatus::Completed => {
                    self.log_event("MCP tool call", format!("{tool} - ok"))?;
                }
                ToolCallStatus::Failed => {
                    self.pending_failed_tool = Some(tool);
                }
            }
            return Ok(());
        }

        if let Some(tool) = self.pending_failed_tool.take() {
            return self.log_event("MCP tool call", format!("{tool} - error: {line}"));
        }

        Ok(())
    }

    fn log_initial_prompt(&mut self, prompt: &str) -> std::io::Result<()> {
        if prompt.trim().is_empty() {
            return self.log_event("Initial prompt", "(empty)");
        }

        for line in prompt.lines() {
            self.log_event("Initial prompt", line)?;
        }
        Ok(())
    }

    fn flush_pending_error(&mut self) {
        if let Some(tool) = self.pending_failed_tool.take() {
            let _ = self.log_event("MCP tool call", format!("{tool} - error"));
        }
    }

    fn log_event(&mut self, label: &str, value: impl AsRef<str>) -> std::io::Result<()> {
        writeln!(
            self.file,
            "{} {label}: {}",
            chrono::Utc::now().to_rfc3339(),
            value.as_ref()
        )?;
        self.file.flush()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolCallStatus {
    Started,
    Completed,
    Failed,
}

fn parse_mcp_tool_status(line: &str) -> Option<(String, ToolCallStatus)> {
    let rest = line.strip_prefix("mcp: ")?;
    let (tool_path, status_text) = if let Some(tool_path) = rest.strip_suffix(" started") {
        (tool_path, ToolCallStatus::Started)
    } else if let Some(tool_path) = rest.strip_suffix(" (completed)") {
        (tool_path, ToolCallStatus::Completed)
    } else if let Some(tool_path) = rest.strip_suffix(" (failed)") {
        (tool_path, ToolCallStatus::Failed)
    } else {
        return None;
    };

    let tool = tool_path.rsplit('/').next()?.to_string();
    Some((tool, status_text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_pty_log_path_contains_role() {
        let role = "executor";
        let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S").to_string();
        let log_path = format!(".ferrus/logs/{}_{}.log", role, ts);
        assert!(log_path.contains(role));
    }

    #[test]
    fn supervisor_task_prompt_names_mode() {
        assert!(supervisor_task_prompt().contains("TASK DEFINITION"));
    }

    #[test]
    fn supervisor_task_prompt_has_hard_rules() {
        assert!(supervisor_task_prompt().contains("HARD RULES"));
    }

    #[test]
    fn supervisor_task_prompt_requires_user_approval_before_create_task() {
        let prompt = supervisor_task_prompt();
        assert!(prompt.contains("explicit user approval"));
        assert!(prompt.contains("do NOT call /create_task until the user has explicitly approved"));
    }

    #[test]
    fn supervisor_plan_prompt_is_freeform() {
        assert!(supervisor_plan_prompt().contains("free-form planning"));
    }

    #[test]
    fn reviewer_prompt_has_hard_rules() {
        assert!(reviewer_prompt().contains("HARD RULES"));
    }

    #[test]
    fn executor_prompt_forbids_manual_checks() {
        assert!(executor_prompt().contains("NEVER"));
    }

    #[test]
    fn executor_resume_prompt_forbids_manual_checks() {
        assert!(executor_resume_prompt().contains("NEVER"));
    }

    #[test]
    fn executor_wait_for_consult_prompt_mentions_tool() {
        assert!(executor_wait_for_consult_prompt().contains("/wait_for_consult"));
    }

    #[test]
    fn consultant_prompt_names_mode() {
        assert!(consultant_prompt().contains("CONSULTATION mode"));
    }

    #[test]
    fn parse_mcp_completed_status_extracts_tool_name() {
        assert_eq!(
            parse_mcp_tool_status("mcp: ferrus-executor-1/check (completed)"),
            Some(("check".to_string(), ToolCallStatus::Completed))
        );
    }

    #[test]
    fn parse_mcp_failed_status_extracts_tool_name() {
        assert_eq!(
            parse_mcp_tool_status("mcp: filesystem/read_mcp_resource (failed)"),
            Some(("read_mcp_resource".to_string(), ToolCallStatus::Failed))
        );
    }

    #[test]
    fn debug_mode_adds_verbose_flag_for_claude_only() {
        let mut claude = StdCommand::new("claude");
        append_debug_agent_flags("claude-code", &mut claude);
        assert_eq!(
            claude
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec!["--verbose".to_string()]
        );

        let mut codex = StdCommand::new("codex");
        append_debug_agent_flags("codex", &mut codex);
        assert!(
            codex.get_args().next().is_none(),
            "codex should not receive an extra debug flag when none is supported"
        );
    }
}
