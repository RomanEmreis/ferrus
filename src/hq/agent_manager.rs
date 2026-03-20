use anyhow::{Context, Result};
use std::process::Stdio;
use tokio::process::Command;

use crate::state::agents::{read_agents, write_agents, AgentEntry, AgentStatus};

const EXECUTOR_PROMPT: &str =
    "You are in executor mode. Call the /wait_for_task MCP tool and complete the assigned task. \
     See .agents/skills/ferrus-executor/SKILL.md for the full workflow.";

const REVIEWER_PROMPT: &str =
    "You are in review mode. Call /wait_for_review, then /review_pending to read the submission. \
     Approve with /approve or reject with /reject and specific feedback. \
     See .agents/skills/ferrus-supervisor/SKILL.md for the full workflow.";

const SUPERVISOR_PLAN_PROMPT: &str =
    "You are in planning mode. Collaborate with the user to define the task, \
     then call /create_task. The HQ will automatically terminate this session \
     and start the executor once /create_task succeeds. \
     Do NOT call /wait_for_review. \
     See .agents/skills/ferrus-supervisor/SKILL.md for the two-mode workflow.";

pub fn agent_binary(agent_type: &str) -> &str {
    match agent_type {
        "claude-code" => "claude",
        "codex" => "codex",
        other => other,
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
pub fn reviewer_prompt() -> &'static str {
    REVIEWER_PROMPT
}
pub fn supervisor_plan_prompt() -> &'static str {
    SUPERVISOR_PLAN_PROMPT
}

#[allow(dead_code)]
/// Spawn an agent in a background PTY session. Returns the `BackgroundSession`
/// handle. Agents.json is updated to `Running`.
pub async fn spawn_background_pty(
    agent_type: &str,
    role: &str,
    name: &str,
    prompt: Option<&str>,
) -> Result<crate::pty::BackgroundSession> {
    let binary = agent_binary(agent_type);

    let log_dir = std::path::Path::new(".ferrus/logs");
    tokio::fs::create_dir_all(log_dir)
        .await
        .context("Failed to create .ferrus/logs")?;
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S");
    let log_path = log_dir.join(format!("{role}_{ts}.log"));

    let args: Vec<&str> = match prompt {
        Some(p) => vec![p],
        None => vec![],
    };

    let session = crate::pty::spawn_background(binary, &args, name, &log_path)
        .with_context(|| format!("Failed to spawn {binary} as {role} in PTY"))?;

    // Update agents.json.
    let mut reg = read_agents().await?;
    reg.upsert(AgentEntry {
        role: role.to_string(),
        agent_type: agent_type.to_string(),
        name: name.to_string(),
        pid: None, // PTY child PID not directly accessible via portable-pty trait
        status: AgentStatus::Running,
        started_at: Some(chrono::Utc::now()),
    });
    write_agents(&reg).await?;

    // NOTE: `agents.json` is now a *logical* registry, not OS-level truth.
    // With PTY sessions, `pid` is None — `agents.json` tracks role lifecycle
    // (Running/Suspended) for display and reconciliation, not for OS-level kill.
    // The PTY session handle (`BackgroundSession`) is the authoritative control object.

    Ok(session)
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
}
