use anyhow::{Context, Result};
use std::path::Path;

use crate::state::{machine::StateData, store};

const DEFAULT_FERRUS_TOML: &str = r#"[checks]
commands = [
    "cargo clippy -- -D warnings",
    "cargo fmt --check",
    "cargo test",
]

[limits]
max_check_retries = 5   # consecutive check failures before state → Failed
max_review_cycles = 3   # reject→fix cycles before state → Failed
max_feedback_lines = 30 # trailing lines per failing command in FEEDBACK.md (full output always in .ferrus/logs/)
wait_timeout_secs = 3600 # how long /wait_for_task and /wait_for_review poll before timing out

[agents]
path = ".agents" # root directory for agent skill files

[lease]
ttl_secs = 90              # how long a claimed lease is valid without renewal
heartbeat_interval_secs = 30 # how often agents should call /heartbeat

[hq]
supervisor = "claude-code"  # agent to use for supervisor/reviewer role: claude-code | codex
executor = "codex"          # agent to use for executor role: claude-code | codex
"#;

const SUPERVISOR_SKILL: &str = r#"---
name: ferrus-supervisor
description: "Use when operating as a Supervisor in a ferrus-orchestrated project — plan mode: create task then exit; review mode: wait_for_review, approve or reject, then exit"
---

# Ferrus Supervisor

You are operating as a **Supervisor** in a ferrus-orchestrated project.
See [ROLE.md](./ROLE.md) for your full role definition and responsibilities.

**Your initial prompt tells you which mode you are in.** Check it before doing anything.

## Plan mode

Your initial prompt says: *"You are in planning mode."*

1. Collaborate with the user to define what needs to be done
2. Call `/create_task` with a detailed Markdown description of what must be done
3. You are done. The HQ automatically terminates this session and starts the executor.
   Do NOT call `/wait_for_review`.

## Review mode

Your initial prompt says: *"You are in review mode."*

1. Call `/wait_for_review` — returns `"status": "claimed"` or `"status": "timeout"`
   - On `"timeout"`: call `/heartbeat` to renew your lease, then call `/wait_for_review` again
   - On `"claimed"`: read `task`, `submission`, `feedback`, and `review` from the returned JSON
2. Call `/review_pending` to read full context (task + submission + state)
3. While reviewing, call `/heartbeat` approximately every 30 seconds to keep your lease alive
4. Call `/approve` to accept, or `/reject` with clear and actionable feedback
5. **Exit.** The HQ handles the next cycle automatically.

## Notes

- Call `/status` at any time to inspect current state and counters
- Call `/ask_human` if you need clarification from a human
- Use the `supervisor-review` MCP prompt for bundled review context
- Read runtime files as MCP resources: `ferrus://task`, `ferrus://submission`, `ferrus://state`
"#;

const SUPERVISOR_ROLE: &str = r#"---
name: ferrus-supervisor-role
description: "Supervisor role definition and boundaries — two modes: plan mode (create task + exit) and review mode (wait_for_review + approve/reject + exit)"
---

# Supervisor Role

You are the **Supervisor** in this ferrus-orchestrated project.

## Two modes — never cross them

The HQ spawns you for exactly one purpose per session. Check your initial prompt:

**Plan mode** ("You are in planning mode"): Collaborate with the user → call `/create_task` → **exit**.

**Review mode** ("You are in review mode"): Call `/wait_for_review` → read context → approve or reject → **exit**.

Never call `/wait_for_review` in plan mode. Never start a new task in review mode.
The HQ drives the full lifecycle; your job is to execute one step and exit.

## Responsibilities

- **Write tasks** — define what must be done with clear acceptance criteria and enough context
- **Review submissions** — inspect the Executor's work and make a single decision
- **Approve** when the work meets all requirements
- **Reject** with specific, actionable notes when it does not

## Boundaries

- You do **not** implement code yourself — delegate all work to the Executor
- Reject only when there is a concrete problem; do not block on preferences not stated in the task
- When state is `Failed` (plan mode only), call `/reset` before creating a new task

## Asking the human

Call `/ask_human` when you need clarification the task description does not cover.
MCP elicitation is used where supported; otherwise state pauses and the human calls `/answer`.
"#;

const EXECUTOR_SKILL: &str = r#"---
name: ferrus-executor
description: "Use when operating as an Executor in a ferrus-orchestrated project — autonomous loop: wait_for_task, implement, heartbeat, check, submit"
---

# Ferrus Executor

You are operating as an **Executor** in a ferrus-orchestrated project.
See [ROLE.md](./ROLE.md) for your full role definition and responsibilities.

## Autonomous loop

1. Call `/wait_for_task` — blocks until a task is assigned; returns JSON with `"status": "claimed"` or `"status": "timeout"`
   - On `"timeout"`: if you still hold a lease, call `/heartbeat` to renew it, then call `/wait_for_task` again
   - On `"claimed"`: read `task`, `feedback`, and `review` from the returned JSON
2. Implement the required changes
3. While working, call `/heartbeat` approximately every 30 seconds to keep your lease alive
4. Call `/check` — fix any failures and repeat until all checks pass
5. Call `/submit` with a summary, manual verification steps, and any known limitations
6. Return to step 1

## Notes

- **NEVER run checks manually** (e.g. `cargo test`, `cargo clippy`, `npm test`). Use `/check` exclusively — it records results, updates state, and handles retry counting. Running checks outside of `/check` wastes time and may mislead you about the actual check state.
- Check failure details are in `.ferrus/FEEDBACK.md`; full logs are in `.ferrus/logs/`
- Call `/status` at any time to inspect current state and counters
- Call `/ask_human` if you need clarification from a human
- Use the `executor-context` MCP prompt for bundled task context
- Read runtime files as MCP resources: `ferrus://task`, `ferrus://feedback`, `ferrus://review`
"#;

const EXECUTOR_ROLE: &str = r#"---
name: ferrus-executor-role
description: "Executor role definition and boundaries — responsibilities, workflow, and constraints for the Executor in a ferrus-orchestrated project"
---

# Executor Role

You are the **Executor** in this ferrus-orchestrated project.

## Responsibilities

- **Implement tasks** faithfully and completely as described in `TASK.md`
- **Run all checks via `/check` only** — never run check commands (e.g. `cargo test`) manually; only `/check` records results and advances state
- **Submit with complete notes** — summary, manual verification steps, known limitations

## Autonomous loop

1. `/wait_for_task` — long-polls until a task is assigned (Executing or re-Addressing after rejection)
2. Read the returned context: task description, any check feedback, any rejection notes
3. Implement the required changes
4. `/check` — fix all failures, repeat until all checks pass
5. `/submit` with full notes
6. Return to step 1

## When re-addressing after rejection

Read `REVIEW.md` carefully. Address **every point** the Supervisor raised before running `/check` again.

## Boundaries

- You do **not** approve your own work — only the Supervisor can
- Do not call `/submit` until `/check` returns a passing result
- Do **not** run check commands manually (e.g. `cargo test`, `cargo clippy`) — use `/check` only
- Do not ignore parts of the task description

## Asking the human

Call `/ask_human` when you encounter ambiguity the task doesn't resolve.
MCP elicitation is used where supported; otherwise state pauses and the human calls `/answer`.
"#;

const FERRUS_SKILL: &str = r#"---
name: ferrus
description: "Use when working on a project that uses ferrus for AI agent orchestration — full tool reference, state machine, resources, prompts, and config"
---

# Ferrus

ferrus is an MCP server that coordinates AI agents in a **Supervisor–Executor** workflow.

## Roles

| Role | Responsibility |
|---|---|
| Supervisor | Writes tasks, reviews Executor submissions, approves or rejects |
| Executor | Implements tasks, runs checks, submits when all checks pass |

Two separate `ferrus serve` processes run side-by-side (one per role), coordinating through `.ferrus/` on disk.

## State machine

```
Idle
 └─► Executing      ← /create_task (Supervisor)
       └─► Checking ← /check (Executor, pass)
             ├─► [FAIL, retries < max] Addressing → /check again
             ├─► [FAIL, retries ≥ max] Failed
             └─► Reviewing ← /submit (Executor)
                   ├─► [REJECT] Addressing → /check loop (retries reset)
                   │     └─► [cycles ≥ max] Failed
                   └─► Complete ← /approve (Supervisor)
```

Any active state can pause to `AwaitingHuman` via `/ask_human`. `/answer` restores it.
`/reset` moves `Failed → Idle`.

## CLI

```sh
ferrus init [--agents-path <path>]              # scaffold project files and skill files
ferrus serve [--role supervisor|executor]       # start MCP server on stdio
ferrus register --supervisor <a> --executor <a> # write MCP config for agents
```

Set `RUST_LOG=ferrus=debug` (or `info`/`warn`) for verbose logs to stderr.

## HQ (run `ferrus` with no arguments)

| Command | Description |
|---|---|
| `/plan` | Spawn supervisor to plan a task, then drive executor→review loop automatically |
| `/review` | Manually spawn supervisor in review mode (if automatic spawning failed) |
| `/attach <name>` | Attach terminal to a running background session (e.g. `executor-1`). Ctrl+] d to detach |
| `/status` | Show task state, agent list, and PTY session log paths |
| `/init` | Initialize ferrus in the current directory |
| `/register` | Register agents |
| `/quit` | Exit HQ |

## MCP tools

### Supervisor
| Tool | From state | Description |
|---|---|---|
| `create_task` | Idle | Write task description; moves to Executing |
| `wait_for_review` | — | Long-poll until state is Reviewing |
| `review_pending` | Reviewing | Read task + submission context |
| `approve` | Reviewing | Accept; moves to Complete |
| `reject` | Reviewing | Reject with notes; moves to Addressing |

### Executor
| Tool | From state | Description |
|---|---|---|
| `wait_for_task` | — | Long-poll until Executing or Addressing |
| `next_task` | Executing, Addressing | Read task + feedback + review notes |
| `check` | Executing, Addressing | Run all configured checks |
| `submit` | Checking | Write submission notes; moves to Reviewing |

### Shared
| Tool | From state | Description |
|---|---|---|
| `ask_human` | any active | Ask human a question (elicitation or AwaitingHuman fallback) |
| `answer` | AwaitingHuman | Provide answer; restores previous state |
| `status` | any | Print current state and counters |
| `reset` | Failed | Return to Idle |
| `heartbeat` | any claimed | Renew lease; returns `{"status":"renewed"}` or `{"status":"error","code":"..."}` |

## MCP resources

| URI | Contents |
|---|---|
| `ferrus://task` | Current task description (`TASK.md`) |
| `ferrus://feedback` | Check failure summary (`FEEDBACK.md`) |
| `ferrus://review` | Supervisor rejection notes (`REVIEW.md`) |
| `ferrus://submission` | Executor submission notes (`SUBMISSION.md`) |
| `ferrus://question` | Pending human question (`QUESTION.md`) |
| `ferrus://state` | Current task state as JSON (`STATE.json`) |

## MCP prompts

| Prompt | Description |
|---|---|
| `executor-context` | State + task + feedback + review notes bundled for the Executor |
| `supervisor-review` | State + task + submission notes bundled for the Supervisor |

## ferrus.toml

```toml
[checks]
commands = ["cargo clippy -- -D warnings", "cargo fmt --check", "cargo test"]

[limits]
max_check_retries = 5    # check failures before Failed
max_review_cycles = 3    # reject→fix cycles before Failed
max_feedback_lines = 30  # lines per command in FEEDBACK.md
wait_timeout_secs = 3600 # poll timeout for wait_for_task / wait_for_review

[lease]
ttl_secs = 90            # lease validity without renewal
heartbeat_interval_secs = 30  # how often to call /heartbeat
```

## Runtime files (`.ferrus/`)

| File | Contents |
|---|---|
| `STATE.json` | State, counters, schema version, timestamp, PID |
| `STATE.lock` | Advisory lock file for atomic claiming |
| `TASK.md` | Task description |
| `FEEDBACK.md` | Check failure summary |
| `REVIEW.md` | Rejection notes |
| `SUBMISSION.md` | Submission notes |
| `QUESTION.md` | Pending human question |
| `ANSWER.md` | Human answer |
| `logs/check_<n>_<ts>.txt` | Full check output |
"#;

pub async fn run(agents_path: String) -> Result<()> {
    create_ferrus_toml(&agents_path).await?;
    create_ferrus_dir().await?;
    create_skill_files(&agents_path).await?;
    update_gitignore().await?;
    println!("\nferrus initialized. Run `ferrus serve` to start the MCP server.");
    Ok(())
}

async fn create_ferrus_toml(agents_path: &str) -> Result<()> {
    let path = Path::new("ferrus.toml");
    if path.exists() {
        println!("ferrus.toml already exists, skipping.");
    } else {
        // Substitute the agents path into the template
        let content = DEFAULT_FERRUS_TOML
            .replace(r#"path = ".agents""#, &format!(r#"path = "{agents_path}""#));
        tokio::fs::write(path, content)
            .await
            .context("Failed to write ferrus.toml")?;
        println!("Created ferrus.toml");
    }
    Ok(())
}

async fn create_ferrus_dir() -> Result<()> {
    let dir = Path::new(".ferrus");
    tokio::fs::create_dir_all(dir.join("logs"))
        .await
        .context("Failed to create .ferrus/logs/ directory")?;

    let state_path = dir.join("STATE.json");
    if !state_path.exists() {
        store::write_state(&StateData::default())
            .await
            .context("Failed to write .ferrus/STATE.json")?;
        println!("Created .ferrus/STATE.json");
    }

    for filename in [
        "TASK.md",
        "FEEDBACK.md",
        "REVIEW.md",
        "SUBMISSION.md",
        "QUESTION.md",
        "ANSWER.md",
    ] {
        let path = dir.join(filename);
        if !path.exists() {
            tokio::fs::write(&path, "")
                .await
                .with_context(|| format!("Failed to write .ferrus/{filename}"))?;
            println!("Created .ferrus/{filename}");
        }
    }

    // Create the advisory lock file used by wait_for_task, wait_for_review, and /heartbeat
    let lock_path = dir.join("STATE.lock");
    if !lock_path.exists() {
        tokio::fs::write(&lock_path, "")
            .await
            .context("Failed to create .ferrus/STATE.lock")?;
        println!("Created .ferrus/STATE.lock");
    }

    // Create empty agents registry
    let agents_path = dir.join("agents.json");
    if !agents_path.exists() {
        let empty = crate::state::agents::AgentsRegistry::default();
        let json = serde_json::to_string_pretty(&empty)?;
        tokio::fs::write(&agents_path, json)
            .await
            .context("Failed to write .ferrus/agents.json")?;
        println!("Created .ferrus/agents.json");
    }

    Ok(())
}

async fn create_skill_files(agents_path: &str) -> Result<()> {
    let skills_root = Path::new(agents_path).join("skills");

    // General ferrus skill
    let ferrus_dir = skills_root.join("ferrus");
    tokio::fs::create_dir_all(&ferrus_dir)
        .await
        .with_context(|| format!("Failed to create {}", ferrus_dir.display()))?;
    let ferrus_skill_path = ferrus_dir.join("SKILL.md");
    if !ferrus_skill_path.exists() {
        tokio::fs::write(&ferrus_skill_path, FERRUS_SKILL)
            .await
            .with_context(|| format!("Failed to write {}", ferrus_skill_path.display()))?;
        println!("Created {}", ferrus_skill_path.display());
    }

    // Role-specific skill + role definition files
    for (role, skill, role_def) in [
        ("ferrus-supervisor", SUPERVISOR_SKILL, SUPERVISOR_ROLE),
        ("ferrus-executor", EXECUTOR_SKILL, EXECUTOR_ROLE),
    ] {
        let skill_dir = skills_root.join(role);
        tokio::fs::create_dir_all(&skill_dir)
            .await
            .with_context(|| format!("Failed to create {}", skill_dir.display()))?;

        let skill_path = skill_dir.join("SKILL.md");
        if !skill_path.exists() {
            tokio::fs::write(&skill_path, skill)
                .await
                .with_context(|| format!("Failed to write {}", skill_path.display()))?;
            println!("Created {}", skill_path.display());
        }

        let role_path = skill_dir.join("ROLE.md");
        if !role_path.exists() {
            tokio::fs::write(&role_path, role_def)
                .await
                .with_context(|| format!("Failed to write {}", role_path.display()))?;
            println!("Created {}", role_path.display());
        }
    }
    Ok(())
}

async fn update_gitignore() -> Result<()> {
    let path = Path::new(".gitignore");
    let entry = ".ferrus/\n";
    if path.exists() {
        let contents = tokio::fs::read_to_string(path)
            .await
            .context("Failed to read .gitignore")?;
        if contents.contains(".ferrus/") {
            return Ok(());
        }
        tokio::fs::write(path, format!("{contents}{entry}"))
            .await
            .context("Failed to update .gitignore")?;
        println!("Added .ferrus/ to .gitignore");
    } else {
        tokio::fs::write(path, entry)
            .await
            .context("Failed to create .gitignore")?;
        println!("Created .gitignore with .ferrus/ entry");
    }
    Ok(())
}
