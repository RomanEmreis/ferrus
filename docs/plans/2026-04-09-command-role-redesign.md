# Command & Role Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `/task`, `/supervisor`, `/executor` commands; rewrite `/plan` as free-form; rename `/execute` → `/resume`; update all prompts and skill files so agents respect their role boundaries.

**Architecture:** All command parsing changes land in `commands.rs`, prompt constants in `agent_manager.rs`, and new handlers in `mod.rs`. A module-level `ResumeGuard` and a `spawn_interactive_agent` helper eliminate duplication across the four interactive-spawn methods. Skill file changes happen in two places: the deployed `.agents/skills/` files (used at runtime) and the string constants in `init.rs` (used when a new project runs `ferrus init`).

**Tech Stack:** Rust, Tokio, Clap, Crossterm (TUI). No new dependencies.

---

## File Map

| File | Change |
|---|---|
| `src/hq/commands.rs` | Add `Task`, `Supervisor`, `Executor` variants; rename `Execute` → `Resume` |
| `src/hq/agent_manager.rs` | Add `SUPERVISOR_TASK_PROMPT`; rewrite all prompt constants; add `supervisor_task_prompt()` |
| `src/hq/mod.rs` | Move `ResumeGuard` to module level; add `spawn_interactive_agent`; rename `execute` → `resume`; rewrite `plan()`; add `task()`, `supervisor_interactive()`, `executor_interactive()`; update dispatch + help |
| `src/cli/commands/init.rs` | Update all 5 skill file string constants |
| `.agents/skills/ferrus-supervisor/SKILL.md` | Rewrite: hard rules first, three named modes |
| `.agents/skills/ferrus-supervisor/ROLE.md` | Rewrite: hard rules first |
| `.agents/skills/ferrus-executor/SKILL.md` | Rewrite: hard rules first, /check rule prominent |
| `.agents/skills/ferrus-executor/ROLE.md` | Rewrite: hard rules first |
| `.agents/skills/ferrus/SKILL.md` | Update HQ command table |
| `CLAUDE.md` | Update Supervisor and Executor sections, command table |
| `AGENTS.md` | Update Supervisor and Executor sections |

---

## Task 1: commands.rs — add variants, rename Execute→Resume

**Files:**
- Modify: `src/hq/commands.rs`

- [ ] **Step 1: Write failing tests for new variants**

Add to the `#[cfg(test)]` block in `src/hq/commands.rs`:

```rust
#[test]
fn parse_task() {
    assert!(matches!(parse_command("/task").unwrap(), ShellCommand::Task));
}
#[test]
fn parse_supervisor_cmd() {
    assert!(matches!(
        parse_command("/supervisor").unwrap(),
        ShellCommand::Supervisor
    ));
}
#[test]
fn parse_executor_cmd() {
    assert!(matches!(
        parse_command("/executor").unwrap(),
        ShellCommand::Executor
    ));
}
#[test]
fn parse_resume() {
    assert!(matches!(
        parse_command("/resume").unwrap(),
        ShellCommand::Resume
    ));
}
#[test]
fn execute_command_removed() {
    assert!(parse_command("/execute").is_err());
}
```

- [ ] **Step 2: Run tests to confirm they fail**

```sh
cargo test parse_task parse_supervisor_cmd parse_executor_cmd parse_resume execute_command_removed 2>&1 | tail -20
```

Expected: compile error — `ShellCommand::Task` etc. do not exist yet. That's correct.

- [ ] **Step 3: Replace the ShellCommand enum and update parse error message**

Replace the entire `ShellCommand` enum and `parse_command` function in `src/hq/commands.rs`:

```rust
#[derive(Debug, Subcommand)]
pub enum ShellCommand {
    /// Show task state and agent list.
    Status,
    /// Reset all task files and set state to Idle (prompts for confirmation if state is Executing or Reviewing).
    Reset,
    /// Stop all running executor and supervisor/reviewer sessions (prompts for confirmation).
    Stop,
    /// Exit HQ.
    Quit,
    /// Free-form planning session with the supervisor (no task created, no state requirement).
    Plan,
    /// Define a task with the supervisor, then run the executor→review loop automatically.
    Task,
    /// Open an interactive supervisor session (no initial prompt, no state requirement).
    Supervisor,
    /// Open an interactive executor session (no initial prompt, no state requirement).
    Executor,
    /// Resume the executor headlessly for the current task (escape hatch).
    Resume,
    /// Attach terminal to a running background session. Ctrl+] d to detach.
    Attach { name: String },
    /// Manually spawn supervisor in review mode (for the current Reviewing submission).
    Review,
    /// Initialize ferrus in the current directory.
    Init {
        #[arg(long, default_value = ".agents")]
        agents_path: String,
    },
    /// Register agents (same as `ferrus register`).
    Register {
        #[arg(long, value_name = "AGENT")]
        supervisor: Option<String>,
        #[arg(long, value_name = "AGENT")]
        executor: Option<String>,
    },
    /// Show all available HQ commands.
    Help,
}

/// Parse `/command [args…]` into a `ShellCommand`.
pub fn parse_command(input: &str) -> Result<ShellCommand> {
    let input = input.trim();
    if !input.starts_with('/') {
        bail!("Commands must start with '/' — try /status, /task, /quit");
    }
    let tokens = shlex::split(&input[1..])
        .ok_or_else(|| anyhow::anyhow!("Failed to tokenize command (unterminated quote?)"))?;
    let cli = HqCli::try_parse_from(tokens).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(cli.command)
}
```

Also update the existing `parse_execute` test to `parse_resume`, and the non_slash_errors test now references `/task`:

```rust
#[test]
fn parse_resume() {   // rename from parse_execute
    assert!(matches!(
        parse_command("/resume").unwrap(),
        ShellCommand::Resume
    ));
}
```

Remove the old `parse_execute` test entirely (replaced by `parse_resume` above and `execute_command_removed`).

- [ ] **Step 4: Fix the compile break in mod.rs (minimal patch)**

`src/hq/mod.rs` still references `ShellCommand::Execute` and calls `ctx.execute()`. Patch both now to keep the code compiling. Full mod.rs work happens in Task 3 — this is only the rename.

In `src/hq/mod.rs`, rename the `execute()` method signature:

```rust
async fn resume(&mut self) -> Result<()> {
```

In the `dispatch()` function's match arm, replace:

```rust
ShellCommand::Execute => ctx.execute().await?,
```

with:

```rust
ShellCommand::Resume => ctx.resume().await?,
```

- [ ] **Step 5: Run tests**

```sh
cargo test 2>&1 | tail -20
```

Expected: all tests pass (commands.rs new tests + all existing tests).

- [ ] **Step 6: Commit**

```sh
git add src/hq/commands.rs src/hq/mod.rs
git commit -m "feat: add Task/Supervisor/Executor commands, rename Execute→Resume"
```

---

## Task 2: agent_manager.rs — new and updated prompt constants

**Files:**
- Modify: `src/hq/agent_manager.rs`

- [ ] **Step 1: Write failing tests for prompt content**

Add to the `#[cfg(test)]` block in `src/hq/agent_manager.rs`:

```rust
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
```

- [ ] **Step 2: Run tests to confirm they fail**

```sh
cargo test supervisor_task_prompt supervisor_plan_prompt_is_freeform reviewer_prompt_has_hard_rules executor_prompt_forbids_manual_checks executor_resume_prompt_forbids_manual_checks 2>&1 | tail -20
```

Expected: compile error — `supervisor_task_prompt` does not exist yet. Correct.

- [ ] **Step 3: Replace all prompt constants and add the new one**

Replace the prompt constants and their accessors at the top of `src/hq/agent_manager.rs`:

```rust
const SUPERVISOR_TASK_PROMPT: &str =
    "You are a Ferrus Supervisor in TASK DEFINITION mode.\n\
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

const SUPERVISOR_PLAN_PROMPT: &str =
    "You are a Ferrus Supervisor in free-form planning mode.\n\
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
```

- [ ] **Step 4: Add the `supervisor_task_prompt()` accessor**

After the existing accessors (`executor_prompt`, `executor_resume_prompt`, `reviewer_prompt`, `supervisor_plan_prompt`), add:

```rust
pub fn supervisor_task_prompt() -> &'static str {
    SUPERVISOR_TASK_PROMPT
}
```

- [ ] **Step 5: Run tests**

```sh
cargo test -p ferrus 2>&1 | tail -20
```

Expected: all `agent_manager` tests pass. `mod.rs` still has compile errors from `ShellCommand::Execute` — fixed in Task 3.

- [ ] **Step 6: Commit**

```sh
git add src/hq/agent_manager.rs
git commit -m "feat: add supervisor_task_prompt, strengthen all agent prompts with hard rules"
```

---

## Task 3: mod.rs — new handlers, renamed execute→resume, updated dispatch

**Files:**
- Modify: `src/hq/mod.rs`

- [ ] **Step 1: Move ResumeGuard to module level**

In `src/hq/mod.rs`, find the `ResumeGuard` struct currently defined inside the `plan()` method body (around line 541) and move it to module level, just before `impl HqContext`. The struct definition is unchanged:

```rust
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
```

Remove the identical definition from inside `plan()`.

- [ ] **Step 2: Add `spawn_interactive_agent` helper to `impl HqContext`**

Add this method to `impl HqContext` (after `spawn_headless_agent`):

```rust
/// Spawn `agent_type` interactively (suspend TUI, inherit stdio, wait for exit, resume TUI).
async fn spawn_interactive_agent(
    &mut self,
    agent_type: &str,
    role: &str,
    name: &str,
    prompt: Option<&str>,
) -> Result<()> {
    use crate::state::agents::{read_agents, write_agents, AgentEntry, AgentStatus};
    use std::process::Stdio;
    use tokio::process::Command;

    let binary = agent_manager::agent_binary(agent_type);
    let mut cmd = Command::new(binary);
    if let Some(p) = prompt {
        cmd.arg(p);
    }

    let ack_rx = self.display.suspend();
    let _ = ack_rx.await;
    let mut guard = ResumeGuard::new(self.display.clone());

    let mut child = cmd
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("Failed to spawn {binary}"))?;

    {
        let mut reg = read_agents().await?;
        reg.upsert(AgentEntry {
            role: role.to_string(),
            agent_type: agent_type.to_string(),
            name: name.to_string(),
            pid: child.id(),
            status: AgentStatus::Running,
            started_at: Some(chrono::Utc::now()),
        });
        write_agents(&reg).await?;
    }

    let _ = child.wait().await;
    guard.resume_now();

    {
        let mut reg = read_agents().await?;
        if let Some(e) = reg.by_role_mut(role) {
            e.pid = None;
            e.status = AgentStatus::Suspended;
        }
        write_agents(&reg).await?;
    }

    Ok(())
}
```

- [ ] **Step 3: Rewrite `plan()` as a free-form session**

Replace the entire `plan()` method body with:

```rust
async fn plan(&mut self) -> Result<()> {
    use crate::config::Config;

    let config = Config::load().await?;
    let hq = config.hq.ok_or_else(|| {
        anyhow::anyhow!(
            "No [hq] section in ferrus.toml. Add:\n[hq]\nsupervisor = \"claude-code\"\nexecutor = \"codex\""
        )
    })?;

    self.supervisor_type = Some(hq.supervisor.clone());
    self.display
        .info(format!("Spawning supervisor ({}) for free-form planning…", hq.supervisor));

    self.spawn_interactive_agent(
        &hq.supervisor,
        "supervisor",
        "supervisor-1",
        Some(agent_manager::supervisor_plan_prompt()),
    )
    .await
}
```

- [ ] **Step 4: Add `task()` method (strict workflow)**

Add this method after `plan()`:

```rust
async fn task(&mut self) -> Result<()> {
    use crate::config::Config;
    use std::process::Stdio;
    use tokio::process::Command;

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
                "State is {other:?} — /task requires Idle or Complete. Use /reset first if needed."
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
    let prompt = agent_manager::supervisor_task_prompt();

    let mut cmd = Command::new(binary);
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
        self.spawn_headless_agent(
            &hq.executor,
            "executor",
            "executor-1",
            agent_manager::executor_prompt(),
        )
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
```

- [ ] **Step 5: Add `supervisor_interactive()` and `executor_interactive()` methods**

Add after `task()`:

```rust
async fn supervisor_interactive(&mut self) -> Result<()> {
    use crate::config::Config;

    let config = Config::load().await?;
    let hq = config.hq.ok_or_else(|| {
        anyhow::anyhow!(
            "No [hq] section in ferrus.toml. Add:\n[hq]\nsupervisor = \"claude-code\"\nexecutor = \"codex\""
        )
    })?;

    self.supervisor_type = Some(hq.supervisor.clone());
    self.display
        .info(format!("Spawning supervisor ({}) interactively…", hq.supervisor));

    self.spawn_interactive_agent(&hq.supervisor, "supervisor", "supervisor-1", None)
        .await
}

async fn executor_interactive(&mut self) -> Result<()> {
    use crate::config::Config;

    let config = Config::load().await?;
    let hq = config.hq.ok_or_else(|| {
        anyhow::anyhow!(
            "No [hq] section in ferrus.toml. Add:\n[hq]\nsupervisor = \"claude-code\"\nexecutor = \"codex\""
        )
    })?;

    self.executor_type = Some(hq.executor.clone());
    self.display
        .info(format!("Spawning executor ({}) interactively…", hq.executor));

    self.spawn_interactive_agent(&hq.executor, "executor", "executor-1", None)
        .await
}
```

- [ ] **Step 6: Add new dispatch arms and update help**

The `execute` → `resume` rename already happened in Task 1. Here, add the new dispatch arms for `/plan`, `/task`, `/supervisor`, `/executor` in `dispatch()`. Replace the existing `ShellCommand::Plan` arm and add the new ones:

```rust
ShellCommand::Plan => ctx.plan().await?,
ShellCommand::Task => ctx.task().await?,
ShellCommand::Supervisor => ctx.supervisor_interactive().await?,
ShellCommand::Executor => ctx.executor_interactive().await?,
ShellCommand::Resume => ctx.resume().await?,
```

Update the `ShellCommand::Help` arm's info string:

```rust
ShellCommand::Help => {
    ctx.display.info(concat!(
        "ferrus HQ commands:\n",
        "  /plan              Free-form planning session with the supervisor\n",
        "  /task              Define a task, then run executor→review loop automatically\n",
        "  /supervisor        Open an interactive supervisor session\n",
        "  /executor          Open an interactive executor session\n",
        "  /review            Manually spawn supervisor in review mode\n",
        "  /resume            Resume the executor headlessly (escape hatch)\n",
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
```

Also update the non-slash-prefix error in `dispatch()`:

```rust
anyhow::bail!("Commands must start with '/' — try /status, /task, /quit");
```

- [ ] **Step 7: Run all tests**

```sh
cargo test 2>&1 | tail -30
```

Expected: all tests pass including the new ones added in Tasks 1 and 2.

- [ ] **Step 8: Run clippy**

```sh
cargo clippy -- -D warnings 2>&1 | tail -20
```

Expected: no warnings.

- [ ] **Step 9: Commit**

```sh
git add src/hq/mod.rs
git commit -m "feat: add /task, /supervisor, /executor commands; rewrite /plan as free-form; rename /execute→/resume"
```

---

## Task 4: init.rs — update skill file templates

**Files:**
- Modify: `src/cli/commands/init.rs`

- [ ] **Step 1: Replace SUPERVISOR_SKILL constant**

Replace the entire `SUPERVISOR_SKILL` constant:

```rust
const SUPERVISOR_SKILL: &str = r#"---
name: ferrus-supervisor
description: "Use when operating as a Supervisor in a ferrus-orchestrated project — task-definition mode: interview user + /create_task; review mode: /wait_for_review + approve/reject; plan mode: free-form planning"
---

# Ferrus Supervisor

Your initial prompt tells you which mode you are in. Match it exactly.

## Hard Rules

In every mode, no exceptions:
- NEVER implement code, edit files, or run shell commands (except in free-form plan mode)
- NEVER call /wait_for_review in task-definition mode
- NEVER call /create_task in review mode

## Task-definition mode

Initial prompt: "You are a Ferrus Supervisor in TASK DEFINITION mode."

1. Interview the user — understand what needs to be done
2. Call `/create_task` with a complete Markdown description
3. Done — HQ terminates this session and spawns the Executor

You do NOT write files. You do NOT implement code. You do NOT explore the codebase
to design a solution. Your sole output is the task description passed to `/create_task`.

## Review mode

Initial prompt: "You are a Ferrus Supervisor in REVIEW mode."

1. Call `/wait_for_review` — on `"timeout"`: `/heartbeat`, retry; on `"claimed"`: read context
2. Call `/review_pending` — reads task + submission
3. Call `/heartbeat` every ~30 seconds while reviewing
4. Call `/approve` or `/reject` with specific feedback
5. Exit — HQ handles the next cycle

You do NOT implement fixes. You do NOT ask the Executor to re-verify.
One decision: `/approve` or `/reject`. Then exit.

## Free-form plan mode

Initial prompt: "You are a Ferrus Supervisor in free-form planning mode."

No hard constraints. Explore, discuss, write plans. `/create_task` is available but not required.

## Notes

- Call `/status` at any time to inspect current state and counters
- Call `/ask_human` if you need clarification from a human
- Use the `supervisor-review` MCP prompt for bundled review context
- Read runtime files as MCP resources: `ferrus://task`, `ferrus://submission`, `ferrus://state`
"#;
```

- [ ] **Step 2: Replace SUPERVISOR_ROLE constant**

```rust
const SUPERVISOR_ROLE: &str = r#"---
name: ferrus-supervisor-role
description: "Supervisor role definition — three modes: task-definition (create task + stop), review (approve/reject + exit), free-form plan (no constraints)"
---

# Supervisor Role

## Hard Rules — read this first

**Task-definition mode:** You do NOT write files, implement code, or run commands.
Your only job is to call `/create_task` with a task description, then stop.

**Review mode:** You do NOT implement fixes. You do NOT ask the Executor to re-verify.
You make one decision — `/approve` or `/reject` — then exit.

## Three modes

**Task-definition** ("TASK DEFINITION mode"): interview → `/create_task` → done
**Review** ("REVIEW mode"): `/wait_for_review` → read context → approve or reject → exit
**Free-form plan** ("free-form planning mode"): no constraints

## Responsibilities

- Write tasks with clear acceptance criteria and enough context for autonomous implementation
- Review submissions and make a single approve/reject decision
- Reject only on concrete problems; do not block on preferences not stated in the task

## Asking the human

Call `/ask_human` when you need clarification. MCP elicitation is used where supported.
"#;
```

- [ ] **Step 3: Replace EXECUTOR_SKILL constant**

```rust
const EXECUTOR_SKILL: &str = r#"---
name: ferrus-executor
description: "Use when operating as an Executor in a ferrus-orchestrated project — autonomous loop: wait_for_task, implement, /check (NEVER manually), submit"
---

# Ferrus Executor

See [ROLE.md](./ROLE.md) for your full role definition.

## Hard Rules — read this first

**NEVER** run check commands manually: no `cargo test`, `cargo clippy`, `cargo fmt`,
`npm test`, `make`, `pytest`, or any equivalent. If you do:
- Results are not recorded in the state machine
- Retry counters are not updated
- `FEEDBACK.md` is not written
- The workflow breaks

**ALWAYS use `/check`** — it is the only correct verification path.
Do not call `/submit` until `/check` returns a passing result.

## Autonomous loop

1. Call `/wait_for_task` — on `"timeout"`: `/heartbeat`, retry; on `"claimed"`: read `task`/`feedback`/`review`
2. Implement the required changes
3. While working, call `/heartbeat` approximately every 30 seconds
4. Call `/check` — read `.ferrus/FEEDBACK.md` for details, fix failures, repeat until all pass
5. Call `/submit` with a summary, manual verification steps, and any known limitations
6. Return to step 1

## When re-addressing after rejection

Read `.ferrus/REVIEW.md`. Address **every point** the Supervisor raised before calling `/check` again.

## Asking the human

1. Call `/ask_human` with your question
2. **Immediately** call `/wait_for_answer` — do not call anything else in between
   - `"answered"`: use the answer and continue
   - `"timeout"`: call `/wait_for_answer` again

You run **headlessly** — no interactive terminal. All human interaction via `/ask_human` + `/wait_for_answer`.

## Notes

- Check failure details: `.ferrus/FEEDBACK.md`; full logs: `.ferrus/logs/`
- Call `/status` at any time to inspect state and counters
- Use the `executor-context` MCP prompt for bundled task context
- Read runtime files: `ferrus://task`, `ferrus://feedback`, `ferrus://review`
"#;
```

- [ ] **Step 4: Replace EXECUTOR_ROLE constant**

```rust
const EXECUTOR_ROLE: &str = r#"---
name: ferrus-executor-role
description: "Executor role definition — implement tasks, use /check exclusively (never manually), submit when all checks pass"
---

# Executor Role

## Hard Rules — read this first

**NEVER** run check commands manually (`cargo test`, `cargo clippy`, `npm test`, etc.).
**ALWAYS** use `/check` — it is the only way to correctly verify your work.

Running checks manually breaks the state machine: results are not recorded, counters
are not updated, `FEEDBACK.md` is not written. The workflow depends on `/check` being
the sole verification path.

**Do not call `/submit` until `/check` returns a passing result.**

## Responsibilities

- Implement tasks faithfully and completely as described in `TASK.md`
- Use `/check` exclusively for all verification
- Submit with a complete summary, verification steps, and known limitations

## Autonomous loop

1. `/wait_for_task` — long-polls until a task is assigned
2. Read the returned context: task, feedback, rejection notes
3. Implement the required changes
4. `/check` — fix all failures, repeat until all pass
5. `/submit` with full notes
6. Return to step 1

## When re-addressing after rejection

Read `REVIEW.md` carefully. Address **every point** before running `/check` again.

## Boundaries

- You do not approve your own work — only the Supervisor can
- You do not run check commands manually
- You do not ignore parts of the task description

## Asking the human

Call `/ask_human` when you encounter ambiguity, then immediately call `/wait_for_answer`.
Do **not** call any other tools in between.

You run **headlessly** — use `/ask_human` + `/wait_for_answer` for all human interaction.
"#;
```

- [ ] **Step 5: Update FERRUS_SKILL constant — HQ section**

In the `FERRUS_SKILL` constant, find the `## HQ (run \`ferrus\` with no arguments)` section and replace the command table:

```rust
## HQ (run `ferrus` with no arguments)

| Command | Description |
|---|---|
| `/plan` | Free-form planning session with the supervisor (no task created) |
| `/task` | Define a task with the supervisor, then run executor→review loop |
| `/supervisor` | Open an interactive supervisor session (no initial prompt) |
| `/executor` | Open an interactive executor session (no initial prompt) |
| `/review` | Manually spawn supervisor in review mode (escape hatch) |
| `/resume` | Resume the executor headlessly (escape hatch) |
| `/status` | Show task state, agent list, and session log paths |
| `/attach <name>` | Show log path for a running headless agent |
| `/stop` | Stop all running agent sessions |
| `/reset` | Reset state to Idle (clears task files) |
| `/init` | Initialize ferrus in the current directory |
| `/register` | Register agent configs |
| `/quit` | Exit HQ |
```

- [ ] **Step 6: Build and test**

```sh
cargo build 2>&1 | tail -10
cargo test 2>&1 | tail -20
```

Expected: clean build, all tests pass.

- [ ] **Step 7: Commit**

```sh
git add src/cli/commands/init.rs
git commit -m "feat: update init.rs skill file templates with hard rules and new commands"
```

---

## Task 5: Update deployed skill files

**Files:**
- Modify: `.agents/skills/ferrus-supervisor/SKILL.md`
- Modify: `.agents/skills/ferrus-supervisor/ROLE.md`
- Modify: `.agents/skills/ferrus-executor/SKILL.md`
- Modify: `.agents/skills/ferrus-executor/ROLE.md`
- Modify: `.agents/skills/ferrus/SKILL.md`

These files are what running agents actually read. They must match the `init.rs` templates exactly (minus the frontmatter which is already correct).

- [ ] **Step 1: Overwrite ferrus-supervisor/SKILL.md**

Replace `.agents/skills/ferrus-supervisor/SKILL.md` with this content (matches `SUPERVISOR_SKILL` from Task 4):

```markdown
---
name: ferrus-supervisor
description: "Use when operating as a Supervisor in a ferrus-orchestrated project — task-definition mode: interview user + /create_task; review mode: /wait_for_review + approve/reject; plan mode: free-form planning"
---

# Ferrus Supervisor

Your initial prompt tells you which mode you are in. Match it exactly.

## Hard Rules

In every mode, no exceptions:
- NEVER implement code, edit files, or run shell commands (except in free-form plan mode)
- NEVER call /wait_for_review in task-definition mode
- NEVER call /create_task in review mode

## Task-definition mode

Initial prompt: "You are a Ferrus Supervisor in TASK DEFINITION mode."

1. Interview the user — understand what needs to be done
2. Call `/create_task` with a complete Markdown description
3. Done — HQ terminates this session and spawns the Executor

You do NOT write files. You do NOT implement code. You do NOT explore the codebase
to design a solution. Your sole output is the task description passed to `/create_task`.

## Review mode

Initial prompt: "You are a Ferrus Supervisor in REVIEW mode."

1. Call `/wait_for_review` — on `"timeout"`: `/heartbeat`, retry; on `"claimed"`: read context
2. Call `/review_pending` — reads task + submission
3. Call `/heartbeat` every ~30 seconds while reviewing
4. Call `/approve` or `/reject` with specific feedback
5. Exit — HQ handles the next cycle

You do NOT implement fixes. You do NOT ask the Executor to re-verify.
One decision: `/approve` or `/reject`. Then exit.

## Free-form plan mode

Initial prompt: "You are a Ferrus Supervisor in free-form planning mode."

No hard constraints. Explore, discuss, write plans. `/create_task` is available but not required.

## Notes

- Call `/status` at any time to inspect current state and counters
- Call `/ask_human` if you need clarification from a human
- Use the `supervisor-review` MCP prompt for bundled review context
- Read runtime files as MCP resources: `ferrus://task`, `ferrus://submission`, `ferrus://state`
```

- [ ] **Step 2: Overwrite ferrus-supervisor/ROLE.md**

Replace `.agents/skills/ferrus-supervisor/ROLE.md`:

```markdown
---
name: ferrus-supervisor-role
description: "Supervisor role definition — three modes: task-definition (create task + stop), review (approve/reject + exit), free-form plan (no constraints)"
---

# Supervisor Role

## Hard Rules — read this first

**Task-definition mode:** You do NOT write files, implement code, or run commands.
Your only job is to call `/create_task` with a task description, then stop.

**Review mode:** You do NOT implement fixes. You do NOT ask the Executor to re-verify.
You make one decision — `/approve` or `/reject` — then exit.

## Three modes

**Task-definition** ("TASK DEFINITION mode"): interview → `/create_task` → done
**Review** ("REVIEW mode"): `/wait_for_review` → read context → approve or reject → exit
**Free-form plan** ("free-form planning mode"): no constraints

## Responsibilities

- Write tasks with clear acceptance criteria and enough context for autonomous implementation
- Review submissions and make a single approve/reject decision
- Reject only on concrete problems; do not block on preferences not stated in the task

## Asking the human

Call `/ask_human` when you need clarification. MCP elicitation is used where supported.
```

- [ ] **Step 3: Overwrite ferrus-executor/SKILL.md**

Replace `.agents/skills/ferrus-executor/SKILL.md`:

```markdown
---
name: ferrus-executor
description: "Use when operating as an Executor in a ferrus-orchestrated project — autonomous loop: wait_for_task, implement, /check (NEVER manually), submit"
---

# Ferrus Executor

See [ROLE.md](./ROLE.md) for your full role definition.

## Hard Rules — read this first

**NEVER** run check commands manually: no `cargo test`, `cargo clippy`, `cargo fmt`,
`npm test`, `make`, `pytest`, or any equivalent. If you do:
- Results are not recorded in the state machine
- Retry counters are not updated
- `FEEDBACK.md` is not written
- The workflow breaks

**ALWAYS use `/check`** — it is the only correct verification path.
Do not call `/submit` until `/check` returns a passing result.

## Autonomous loop

1. Call `/wait_for_task` — on `"timeout"`: `/heartbeat`, retry; on `"claimed"`: read `task`/`feedback`/`review`
2. Implement the required changes
3. While working, call `/heartbeat` approximately every 30 seconds
4. Call `/check` — read `.ferrus/FEEDBACK.md` for details, fix failures, repeat until all pass
5. Call `/submit` with a summary, manual verification steps, and any known limitations
6. Return to step 1

## When re-addressing after rejection

Read `.ferrus/REVIEW.md`. Address **every point** the Supervisor raised before calling `/check` again.

## Asking the human

1. Call `/ask_human` with your question
2. **Immediately** call `/wait_for_answer` — do not call anything else in between
   - `"answered"`: use the answer and continue
   - `"timeout"`: call `/wait_for_answer` again

You run **headlessly** — no interactive terminal. All human interaction via `/ask_human` + `/wait_for_answer`.

## Notes

- Check failure details: `.ferrus/FEEDBACK.md`; full logs: `.ferrus/logs/`
- Call `/status` at any time to inspect state and counters
- Use the `executor-context` MCP prompt for bundled task context
- Read runtime files: `ferrus://task`, `ferrus://feedback`, `ferrus://review`
```

- [ ] **Step 4: Overwrite ferrus-executor/ROLE.md**

Replace `.agents/skills/ferrus-executor/ROLE.md`:

```markdown
---
name: ferrus-executor-role
description: "Executor role definition — implement tasks, use /check exclusively (never manually), submit when all checks pass"
---

# Executor Role

## Hard Rules — read this first

**NEVER** run check commands manually (`cargo test`, `cargo clippy`, `npm test`, etc.).
**ALWAYS** use `/check` — it is the only way to correctly verify your work.

Running checks manually breaks the state machine: results are not recorded, counters
are not updated, `FEEDBACK.md` is not written. The workflow depends on `/check` being
the sole verification path.

**Do not call `/submit` until `/check` returns a passing result.**

## Responsibilities

- Implement tasks faithfully and completely as described in `TASK.md`
- Use `/check` exclusively for all verification
- Submit with a complete summary, verification steps, and known limitations

## Autonomous loop

1. `/wait_for_task` — long-polls until a task is assigned
2. Read the returned context: task, feedback, rejection notes
3. Implement the required changes
4. `/check` — fix all failures, repeat until all pass
5. `/submit` with full notes
6. Return to step 1

## When re-addressing after rejection

Read `REVIEW.md` carefully. Address **every point** before running `/check` again.

## Boundaries

- You do not approve your own work — only the Supervisor can
- You do not run check commands manually
- You do not ignore parts of the task description

## Asking the human

Call `/ask_human` when you encounter ambiguity, then immediately call `/wait_for_answer`.
Do **not** call any other tools in between.

You run **headlessly** — use `/ask_human` + `/wait_for_answer` for all human interaction.
```

- [ ] **Step 5: Update ferrus/SKILL.md HQ command table**

In `.agents/skills/ferrus/SKILL.md`, replace the `## HQ (run \`ferrus\` with no arguments)` section's command table:

```markdown
## HQ (run `ferrus` with no arguments)

| Command | Description |
|---|---|
| `/plan` | Free-form planning session with the supervisor (no task created) |
| `/task` | Define a task with the supervisor, then run executor→review loop |
| `/supervisor` | Open an interactive supervisor session (no initial prompt) |
| `/executor` | Open an interactive executor session (no initial prompt) |
| `/review` | Manually spawn supervisor in review mode (escape hatch) |
| `/resume` | Resume the executor headlessly (escape hatch) |
| `/status` | Show task state, agent list, and session log paths |
| `/attach <name>` | Show log path for a running headless agent |
| `/stop` | Stop all running agent sessions |
| `/reset` | Reset state to Idle (clears task files) |
| `/init` | Initialize ferrus in the current directory |
| `/register` | Register agent configs |
| `/quit` | Exit HQ |
```

- [ ] **Step 6: Commit**

```sh
git add .agents/skills/
git commit -m "feat: rewrite skill files with hard rules first and updated command names"
```

---

## Task 6: Update CLAUDE.md and AGENTS.md

**Files:**
- Modify: `CLAUDE.md`
- Modify: `AGENTS.md`

- [ ] **Step 1: Update CLAUDE.md — HQ shell commands table**

In `CLAUDE.md`, find the `### HQ shell commands` table and replace it:

```markdown
### HQ shell commands

| Command | Description |
|---|---|
| `/plan` | Free-form planning session with the supervisor (no task created) |
| `/task` | Define a task with the supervisor, then drive executor→review loop automatically |
| `/supervisor` | Open an interactive supervisor session (no initial prompt) |
| `/executor` | Open an interactive executor session (no initial prompt) |
| `/review` | Manually spawn supervisor in review mode (escape hatch when automatic spawning failed) |
| `/resume` | Resume the executor headlessly (escape hatch if automatic spawning failed) |
| `/status` | Show task state, agent list, and session log paths |
| `/attach <name>` | Show log path for a running headless agent |
| `/stop` | Stop all running agent sessions (prompts for confirmation) |
| `/reset` | Reset state to Idle and clear task files (prompts for confirmation) |
| `/init [--agents-path]` | Initialize ferrus in the current directory |
| `/register` | Register agent configs (same as `ferrus register`) |
| `/help` | List all HQ commands |
| `/quit` | Exit HQ |
```

- [ ] **Step 2: Update CLAUDE.md — Ferrus Supervisor section**

Replace the `## Ferrus Supervisor` section at the bottom of CLAUDE.md:

```markdown
## Ferrus Supervisor

This repository is orchestrated by Ferrus HQ.

The Supervisor runs in one of three modes — check your initial prompt:

**Task-definition mode** ("You are a Ferrus Supervisor in TASK DEFINITION mode"): Interview the user to understand what needs to be done, then call `/create_task`. Do NOT write any files or implement any code. The HQ automatically terminates this session once `/create_task` succeeds — you do not need to exit. Do NOT call `/wait_for_review`.

**Review mode** ("You are a Ferrus Supervisor in REVIEW mode"): Call `/wait_for_review`, then `/review_pending` to read TASK.md + SUBMISSION.md, then `/approve` or `/reject`. Do NOT implement any fixes. After deciding, **exit**.

**Free-form plan mode** ("You are a Ferrus Supervisor in free-form planning mode"): No constraints — explore, discuss, write plans. `/create_task` is available but not required.

See `.agents/skills/ferrus-supervisor/SKILL.md` for the full three-mode workflow.
```

- [ ] **Step 3: Update AGENTS.md — Ferrus Supervisor section**

Replace the `## Ferrus Supervisor` section in AGENTS.md:

```markdown
## Ferrus Supervisor

This repository is orchestrated by Ferrus HQ.

The Supervisor runs in one of three modes — check your initial prompt:

**Task-definition mode** ("You are a Ferrus Supervisor in TASK DEFINITION mode"): Interview the user to understand what needs to be done, then call `/create_task`. Do NOT write any files, run any commands, or implement any code. The HQ automatically terminates this session once `/create_task` succeeds.

**Review mode** ("You are a Ferrus Supervisor in REVIEW mode"): Call `/wait_for_review`, then `/review_pending` to read TASK.md + SUBMISSION.md, then `/approve` or `/reject`. Do NOT implement any fixes or ask the Executor to re-verify. After deciding, **exit**.

**Free-form plan mode** ("You are a Ferrus Supervisor in free-form planning mode"): No constraints — explore, discuss, write plans. `/create_task` is available but not required.

See `.agents/skills/ferrus-supervisor/SKILL.md` for the full three-mode workflow.
```

- [ ] **Step 4: Update AGENTS.md — Ferrus Executor section**

Replace the `## Ferrus Executor` section in AGENTS.md:

```markdown
## Ferrus Executor

This repository is orchestrated by Ferrus HQ.

When spawned by `ferrus` HQ, your initial prompt will tell you what to do.

If started manually: call MCP tool `/wait_for_task` as your first action.

**HARD RULE**: Never run check commands manually (e.g. `cargo test`, `cargo clippy`, `npm test`, `make`). Always use the `/check` MCP tool — it records results, updates state, and handles retry counting. Running checks outside of `/check` bypasses the state machine entirely: retry counters are not updated, FEEDBACK.md is not written, and state transitions are skipped.

Full workflow: `.agents/skills/ferrus-executor/SKILL.md`
```

- [ ] **Step 5: Run full test suite and clippy**

```sh
cargo test 2>&1 | tail -20
cargo clippy -- -D warnings 2>&1 | tail -10
cargo fmt --check 2>&1
```

Expected: all pass, no warnings, formatting clean.

- [ ] **Step 6: Commit**

```sh
git add CLAUDE.md AGENTS.md
git commit -m "docs: update CLAUDE.md and AGENTS.md with three-mode supervisor, hard rules, new commands"
```
