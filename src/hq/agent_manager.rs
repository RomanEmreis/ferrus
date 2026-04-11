use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

use crate::agent_id::{ROLE_EXECUTOR, ROLE_SUPERVISOR};
use crate::agents::{ExecutorAgent, SupervisorAgent};
use crate::state::agents::{read_agents, write_agents, AgentEntry, AgentStatus};

const SUPERVISOR_TASK_PROMPT: &str = "You are a Ferrus Supervisor in TASK DEFINITION mode.\n\
     \n\
     YOUR ONLY JOB: Interview the user about what needs to be done, then call /create_task \
     with a complete task description. The HQ terminates this session automatically once \
     /create_task succeeds and hands off to the Executor.\n\
     \n\
     HARD RULES — no exceptions:\n\
       - DO NOT write, edit, or create any files\n\
       - DO NOT run any commands or implement any code\n\
       - DO NOT explore the codebase to design a solution yourself\n\
       - DO NOT ask the Executor to verify anything — it does not exist yet\n\
       - Call /create_task as soon as you have enough information; never implement first\n\
     \n\
     After /create_task succeeds you are done. The HQ handles everything else.\n\
     See .agents/skills/ferrus-supervisor/SKILL.md for the full workflow.";

const SUPERVISOR_PLAN_PROMPT: &str = "You are a Ferrus Supervisor in free-form planning mode.\n\
     \n\
     Explore the codebase, discuss ideas, and help the user plan. You are NOT required to \
     call /create_task — this is a freeform planning conversation. Use ferrus MCP tools \
     (e.g. /status) as needed. There are no hard constraints on what you may do.\n\
     \n\
     See .agents/skills/ferrus-supervisor/SKILL.md for context on the ferrus workflow.";

const REVIEWER_PROMPT: &str =
    "You are a Ferrus Supervisor in REVIEW mode.\n\
     \n\
     Call /wait_for_review, then /review_pending to read the submission. Make one decision: \
     /approve or /reject with specific feedback. Then exit — the HQ handles the next cycle.\n\
     \n\
     HARD RULES — no exceptions:\n\
       - DO NOT implement any fixes or changes yourself\n\
       - DO NOT ask the Executor to re-verify — the submission is already in; your job is to judge it\n\
       - Make exactly one decision: /approve or /reject\n\
     \n\
     See .agents/skills/ferrus-supervisor/SKILL.md for the full workflow.";

const EXECUTOR_PROMPT: &str =
    "You are a Ferrus Executor. Call /wait_for_task, implement the assigned task, then submit.\n\
     \n\
     HARD RULES — no exceptions:\n\
       - NEVER run cargo, npm, make, pytest, or any check/build/test command manually\n\
       - ALWAYS use /check for all verification — it records results, updates state, and \
         handles retry counting; running checks manually bypasses the state machine entirely\n\
       - If you are stuck, blocked, or need human input, you MUST use /ask_human\n\
       - Do NOT ask questions in the terminal — you are running headlessly and no one will see them\n\
       - The /ask_human flow is the only supported channel for human communication\n\
     \n\
     See .agents/skills/ferrus-executor/SKILL.md for the full workflow.";

const EXECUTOR_RESUME_PROMPT: &str =
    "You are a Ferrus Executor being relaunched after a human answered your question. \
     The answer is in .ferrus/ANSWER.md — read it, then continue your work. \
     Call /wait_for_task and resume the assigned task from where you left off.\n\
     \n\
     HARD RULES — no exceptions:\n\
       - NEVER run cargo, npm, make, pytest, or any check/build/test command manually\n\
       - ALWAYS use /check for all verification\n\
       - If you are stuck, blocked, or need human input, you MUST use /ask_human\n\
       - Do NOT ask questions in the terminal — you are running headlessly and no one will see them\n\
       - The /ask_human flow is the only supported channel for human communication\n\
     \n\
     See .agents/skills/ferrus-executor/SKILL.md for the full workflow.";

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
pub fn reviewer_prompt() -> &'static str {
    REVIEWER_PROMPT
}
pub fn supervisor_plan_prompt() -> &'static str {
    SUPERVISOR_PLAN_PROMPT
}
pub fn supervisor_task_prompt() -> &'static str {
    SUPERVISOR_TASK_PROMPT
}

/// Handle for a headless background executor process.
pub struct HeadlessHandle {
    #[allow(dead_code)]
    pub name: String,
    pub log_path: PathBuf,
    pub pid: u32,
    pub exit_rx: tokio::sync::watch::Receiver<Option<i32>>,
    wait_thread: Option<std::thread::JoinHandle<()>>,
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
        #[cfg(unix)]
        unsafe {
            libc::kill(self.pid as libc::pid_t, signal);
        }
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
) -> Result<HeadlessHandle> {
    spawn_headless(agent.name(), agent.spawn_headlessly(prompt), ROLE_EXECUTOR, name).await
}

pub async fn spawn_headless_supervisor(
    agent: &dyn SupervisorAgent,
    name: &str,
    prompt: &str,
) -> Result<HeadlessHandle> {
    spawn_headless(agent.name(), agent.spawn_headlessly(prompt), ROLE_SUPERVISOR, name).await
}

async fn spawn_headless(
    agent_type: &str,
    mut command: std::process::Command,
    role: &str,
    name: &str,
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
    let log_stderr = log_file
        .try_clone()
        .context("Failed to clone log file handle")?;

    // On Linux: ask the kernel to send SIGTERM to this child whenever ferrus exits,
    // even if ferrus is killed with SIGKILL and cannot run its own cleanup.
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: prctl(PR_SET_PDEATHSIG) is async-signal-safe and operates only on
        // the calling process. It is safe to call between fork and exec.
        unsafe {
            command.pre_exec(|| {
                libc::prctl(
                    libc::PR_SET_PDEATHSIG,
                    libc::SIGTERM as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                );
                Ok(())
            });
        }
    }

    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_stderr))
        .spawn()
        .with_context(|| {
            format!(
                "Failed to spawn {} headlessly as {role}",
                command.get_program().to_string_lossy()
            )
        })?;

    let pid = child.id();

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
    let wait_thread = std::thread::spawn(move || {
        let code = child.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
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
    })
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
}
