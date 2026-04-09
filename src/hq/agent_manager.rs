use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;

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
       - Do not call /submit until /check returns a passing result\n\
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
       - Do not call /submit until /check returns a passing result\n\
     \n\
     See .agents/skills/ferrus-executor/SKILL.md for the full workflow.";

pub fn agent_binary(agent_type: &str) -> &str {
    match agent_type {
        "claude-code" => "claude",
        "codex" => "codex",
        other => other,
    }
}

/// Extra CLI args to activate the agent's native plan mode.
/// Returns an empty slice for agents that have no such mode (graceful fallback).
#[allow(dead_code)]
pub fn plan_mode_args(agent_type: &str) -> &'static [&'static str] {
    match agent_type {
        "claude-code" => &["--permission-mode", "plan"],
        _ => &[],
    }
}

/// Returns the initial prompt as a single-element vec, or empty if none.
/// Both `claude` and `codex` accept the initial message as the first positional arg.
#[allow(dead_code)]
pub fn initial_prompt_arg(prompt: Option<&str>) -> Vec<&str> {
    match prompt {
        Some(p) => vec![p],
        None => vec![],
    }
}

#[allow(dead_code)]
/// Spawn an agent with inherited stdio and wait for it to exit.
/// Returns the exit code.
pub async fn spawn_and_wait(
    agent_type: &str,
    role: &str,
    name: &str,
    prompt: Option<&str>,
) -> Result<i32> {
    let binary = agent_binary(agent_type);
    let mut cmd = Command::new(binary);
    if let Some(p) = prompt {
        cmd.arg(p);
    }
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    // Register as Running before spawn.
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

    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to spawn {binary} as {role}"))?;

    // Update PID now that we have it.
    let pid = child.id();
    let mut reg = read_agents().await?;
    if let Some(e) = reg.by_role_mut(role) {
        e.pid = pid;
    }
    write_agents(&reg).await?;

    let status = child
        .wait()
        .await
        .with_context(|| format!("{binary} process error"))?;

    // Mark as Suspended after exit.
    let mut reg = read_agents().await?;
    if let Some(e) = reg.by_role_mut(role) {
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
            // SAFETY: kill(pid, SIGTERM) is a well-defined syscall.
            // We accept that the PID might be stale — kill returns ESRCH in that case,
            // which we intentionally ignore.
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

/// CLI args that run the agent non-interactively with `prompt` as the initial message.
/// - Codex:       `codex exec "<prompt>"`
/// - Claude Code: `claude -p "<prompt>"`   (non-interactive / headless mode)
/// - Other:       `<prompt>` as a positional arg (fallback; may not work for all agents)
pub fn headless_args(agent_type: &str, prompt: &str) -> Vec<String> {
    match agent_type {
        "codex" => vec!["exec".to_string(), prompt.to_string()],
        "claude-code" => vec!["-p".to_string(), prompt.to_string()],
        _ => vec![prompt.to_string()],
    }
}

/// Handle for a headless background executor process.
pub struct HeadlessHandle {
    #[allow(dead_code)] // retained for future display / diagnostics use
    pub name: String,
    pub log_path: PathBuf,
    /// OS PID of the child process, used for liveness checks and SIGTERM.
    pub pid: u32,
    /// `None` = alive, `Some(code)` = exited.
    pub exit_rx: tokio::sync::watch::Receiver<Option<i32>>,
}

impl HeadlessHandle {
    pub fn is_alive(&self) -> bool {
        self.exit_rx.borrow().is_none()
    }

    /// Send SIGTERM to the headless process. No-op if the process has already exited.
    pub fn kill(&self) {
        #[cfg(unix)]
        // SAFETY: kill(pid, SIGTERM) is a well-defined syscall. We intentionally ignore
        // ESRCH (process already gone) and other errors — this is best-effort cleanup.
        unsafe {
            libc::kill(self.pid as libc::pid_t, libc::SIGTERM);
        }
    }
}

/// Spawn `agent_type` headlessly (no PTY, no TUI).
/// Stdout and stderr are both streamed to `.ferrus/logs/{role}_{ts}.log`.
/// Returns a `HeadlessHandle` for lifecycle tracking.
/// `agents.json` is updated to `Running` immediately.
pub async fn spawn_headless(
    agent_type: &str,
    role: &str,
    name: &str,
    prompt: &str,
) -> Result<HeadlessHandle> {
    let binary = agent_binary(agent_type);

    let log_dir = std::path::Path::new(".ferrus/logs");
    tokio::fs::create_dir_all(log_dir)
        .await
        .context("Failed to create .ferrus/logs")?;
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S");
    let log_path = log_dir.join(format!("{role}_{ts}.log"));

    // Open log file; clone the handle so both stdout and stderr point to it.
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("Failed to open log file {}", log_path.display()))?;
    let log_stderr = log_file
        .try_clone()
        .context("Failed to clone log file handle")?;

    let args = headless_args(agent_type, prompt);
    let mut cmd = Command::new(binary);
    for arg in &args {
        cmd.arg(arg);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_stderr));

    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to spawn {binary} headlessly as {role}"))?;

    let pid = child.id().unwrap_or(0);

    // Update agents.json: Running with real PID.
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

    // Background task: wait for exit, update agents.json, notify via watch channel.
    let (exit_tx, exit_rx) = tokio::sync::watch::channel::<Option<i32>>(None);
    let role_owned = role.to_string();
    tokio::spawn(async move {
        let code = child
            .wait()
            .await
            .map(|s| s.code().unwrap_or(-1))
            .unwrap_or(-1);
        let _ = exit_tx.send(Some(code));

        if let Ok(mut reg) = read_agents().await {
            if let Some(e) = reg.by_role_mut(&role_owned) {
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_for_claude_code() {
        assert_eq!(agent_binary("claude-code"), "claude");
    }
    #[test]
    fn binary_for_codex() {
        assert_eq!(agent_binary("codex"), "codex");
    }
    #[test]
    fn binary_passthrough_for_unknown() {
        assert_eq!(agent_binary("my-agent"), "my-agent");
    }
    #[test]
    fn plan_mode_args_claude_code() {
        assert_eq!(
            plan_mode_args("claude-code"),
            &["--permission-mode", "plan"]
        );
    }
    #[test]
    fn plan_mode_args_codex_is_empty() {
        assert!(plan_mode_args("codex").is_empty());
    }
    #[test]
    fn plan_mode_args_unknown_is_empty() {
        assert!(plan_mode_args("my-agent").is_empty());
    }
    #[test]
    fn no_prompt_gives_empty_args() {
        assert!(initial_prompt_arg(None).is_empty());
    }
    #[test]
    fn prompt_becomes_first_arg() {
        assert_eq!(initial_prompt_arg(Some("do it")), vec!["do it"]);
    }
    #[test]
    fn background_pty_log_path_contains_role() {
        // Regression: log path must embed the role name for easy grepping.
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
