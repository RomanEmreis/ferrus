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

Any active state can pause to `AwaitingHuman` via `/ask_human` (executor then calls `/wait_for_answer`
to block until the human responds). The human types their answer in the HQ terminal.
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
| `ask_human` | Executing, Addressing, Checking, Reviewing | Write question to QUESTION.md; moves to AwaitingHuman. Call `/wait_for_answer` immediately after. |
| `wait_for_answer` | AwaitingHuman | Block until the human answers; restores previous state and returns the answer |

### Shared
| Tool | From state | Description |
|---|---|---|
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
    let entries = [
        ".ferrus/",
        ".claude/settings.local.json",
        ".claude/.mcp.json",
        ".mcp.json",
        ".codex/config.toml",
    ];

    if path.exists() {
        let mut contents = tokio::fs::read_to_string(path)
            .await
            .context("Failed to read .gitignore")?;

        let mut added_entries = Vec::new();
        for entry in entries {
            if contents.lines().any(|line| line == entry) {
                continue;
            }

            if !contents.is_empty() && !contents.ends_with('\n') {
                contents.push('\n');
            }
            contents.push_str(entry);
            contents.push('\n');
            added_entries.push(entry);
        }

        if added_entries.is_empty() {
            return Ok(());
        }

        tokio::fs::write(path, contents)
            .await
            .context("Failed to update .gitignore")?;

        for entry in added_entries {
            println!("Added {entry} to .gitignore");
        }
    } else {
        let contents = format!("{}\n", entries.join("\n"));
        tokio::fs::write(path, contents)
            .await
            .context("Failed to create .gitignore")?;
        println!("Created .gitignore");
    }
    Ok(())
}
