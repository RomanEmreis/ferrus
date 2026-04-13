use anyhow::{Context, Result};
use std::path::Path;

use crate::state::{machine::StateData, store};

const DEFAULT_FERRUS_TOML: &str = r#"[checks]
commands = []

[limits]
max_check_retries = 20  # consecutive check failures before state → Failed
max_review_cycles = 3   # reject→fix cycles before state → Failed
max_feedback_lines = 30 # trailing lines per failing command shown in /check and /submit output (full output always in .ferrus/logs/)
wait_timeout_secs = 60 # max duration of a single wait_* tool call before it returns timeout so the agent can poll again

[agents]
path = ".agents" # root directory for agent skill files

[lease]
ttl_secs = 90              # how long a claimed lease is valid without renewal
heartbeat_interval_secs = 30 # how often agents should call /heartbeat

[hq.supervisor]
agent = "claude-code"  # agent to use for supervisor/reviewer role: claude-code | codex
model = ""             # optional override; empty = agent default

[hq.executor]
agent = "codex"        # agent to use for executor role: claude-code | codex
model = ""             # optional override; empty = agent default
"#;

const SUPERVISOR_SKILL: &str = r#"---
name: ferrus-supervisor
description: "Advisory Supervisor playbook for task drafting, review, and consultation quality"
---

# Supervisor Operating Playbook

This file is advisory only.
Runtime workflow is defined by the initial prompt and Ferrus MCP tools.

## Task drafting

- Define the expected outcome clearly
- State relevant constraints and acceptance criteria
- Keep task scope explicit and bounded
- Draft task text that the user can review directly

## Review quality

- Judge correctness against the task, not personal preference
- Focus on regressions, missing requirements, and verification gaps
- Write rejection feedback that is concrete and actionable

## Consultation quality

- Answer the Executor's actual blocker
- Prefer concrete direction over abstract discussion
- Clarify tradeoffs when there is no single obvious answer

## Human interaction

- Confirm task wording with the user before task creation
- Use `/ask_human` only when the answer cannot be reliably derived from the repository or current context

## Useful Ferrus tools

- `/create_task`
- `/wait_for_review`
- `/review_pending`
- `/approve`
- `/reject`
- `/respond_consult`
- `/ask_human`

## Useful Ferrus resources

- `ferrus://task`
- `ferrus://submission`
- `ferrus://review`
- `ferrus://consult_request`
"#;

const SUPERVISOR_ROLE: &str = r#"---
name: ferrus-supervisor-role
description: "High-level Supervisor role description and boundaries"
---

# Supervisor Role

High-level description of the Supervisor role.

## Responsibilities

- Define clear, executable tasks
- Review submitted work
- Provide consultation when the Executor is blocked

## Boundaries

- Does not implement Executor work in task-definition or review mode
- Does not bypass Ferrus tools or state transitions
- Does not manipulate `.ferrus/` files to force progress

## Notes

This file is descriptive only.
Runtime behavior is defined by the initial prompt and Ferrus MCP tools.
If this file conflicts with them, follow the prompt and tools.
"#;

const EXECUTOR_SKILL: &str = r#"---
name: ferrus-executor
description: "Advisory Executor playbook for implementation, code navigation, and submission quality"
---

# Executor Operating Playbook

This file is advisory only.
Runtime workflow is defined by the initial prompt and Ferrus MCP tools.

## Implementation guidelines

- Prefer minimal, targeted diffs
- Avoid unrelated refactoring
- Preserve existing project patterns unless the task requires otherwise

## Code navigation

- Start from entrypoints and public interfaces
- Trace dependencies before changing behavior
- Inspect surrounding code before modifying shared logic

## Common pitfalls

- Hidden side effects
- Implicit contracts between modules
- Test coupling and fixture assumptions
- State transitions that depend on tool behavior

## Ferrus guidance

- Use Ferrus tools rather than reconstructing state from `.ferrus/`
- Use `/check` freely during development; prefer TDD where it fits the task
- Run `/check` again immediately before the final `/submit`
- Read Ferrus resources when they help clarify task context
- Use the consultation template when escalating technical uncertainty

## Submission quality

- Provide a clear summary of what changed
- Include concrete manual verification steps
- Mention limitations or follow-up work explicitly when relevant

## Useful Ferrus tools

- `/wait_for_task`
- `/check`
- `/consult`
- `/wait_for_consult`
- `/ask_human`
- `/wait_for_answer`
- `/submit`

## Useful Ferrus resources

- `ferrus://task`
- `ferrus://review`
- `ferrus://consult_template`
- `ferrus://question`
- `ferrus://answer`
"#;

const CONSULT_TEMPLATE: &str = r#"## Problem
...

## What I tried
...

## Options (if any)
...

## Question
...
"#;

const EXECUTOR_ROLE: &str = r#"---
name: ferrus-executor-role
description: "High-level Executor role description and boundaries"
---

# Executor Role

High-level description of the Executor role.

## Responsibilities

- Implement assigned tasks
- Verify work via `/check`
- Submit completed results via `/submit`

## Boundaries

- Does not approve own work
- Does not redefine the task
- Does not bypass Ferrus tools or state transitions
- Does not emulate Ferrus tool effects by editing `.ferrus/` directly

## Notes

This file is descriptive only.
Runtime behavior is defined by the initial prompt and Ferrus MCP tools.
If this file conflicts with them, follow the prompt and tools.
"#;

const FERRUS_SKILL: &str = r#"---
name: ferrus
description: "Use when working on a project that uses ferrus for AI agent orchestration — full tool reference, state machine, resources, prompts, and config"
---

# Ferrus

ferrus is an MCP server that coordinates AI agents in a **Supervisor–Executor** workflow.

This file is supporting context only.
Runtime behavior is defined by the active initial prompt and Ferrus MCP tools.
If this file conflicts with them, follow the prompt and tools.

## Roles

| Role | Responsibility |
|---|---|
| Supervisor | Writes tasks, reviews Executor submissions, approves or rejects |
| Executor | Implements tasks, runs checks during development, and submits when ready |

Two separate `ferrus serve` processes run side-by-side (one per role), coordinating through `.ferrus/` on disk.

Under HQ, agents are usually **one-shot sessions**:
- Executor starts on `Idle → Executing`, claims work via `wait_for_task`, implements, uses `/check` as needed during development, runs `/check` again immediately before final handoff, then calls `/submit` for the final review gate
- HQ then terminates that Executor session and starts the Supervisor in review mode
- If review is rejected, HQ terminates the reviewer and starts a fresh Executor session for `Addressing`
- That new Executor begins again with `wait_for_task` and receives the latest review context

## State machine

```
Idle
 └─► Executing      ← /create_task (Supervisor)
       ├─► Addressing ← /reject (Supervisor) → work loop
       ├─► Consultation ← /consult (Executor)
       │     └─► (restore previous state) ← /wait_for_consult
       ├─► Reviewing ← /submit final gate pass (Executor)
       │     ├─► [REJECT] Addressing → work loop
       │     └─► Complete ← /approve (Supervisor)
       └─► Failed   ← /check or /submit hits retry limit
```

Any active Executor work state can pause to `Consultation` via `/consult` (executor then calls `/wait_for_consult`
to block until the Supervisor responds via `/respond_consult`, which records `CONSULT_RESPONSE.md`).
Any active state, including `Consultation`, can pause to `AwaitingHuman` via `/ask_human` (executor then calls `/wait_for_answer`
to block until the human responds). The human types their answer in the HQ terminal.
`/reset` moves `Failed → Idle`.

## CLI

```sh
ferrus init [--agents-path <path>]              # scaffold project files and skill files
ferrus serve [--role supervisor|executor]       # start MCP server on stdio
ferrus register --supervisor <a> --supervisor-model <m> --executor <a> --executor-model <m> # write MCP config for agents
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
| `/resume` | Resume the executor headlessly; also recovers Consultation by relaunching both consultant and executor |
| `/status` | Show task state, agent list, and session log paths |
| `/attach <name>` | Show log path for a running headless agent |
| `/stop` | Stop all running agent sessions |
| `/reset` | Reset state to Idle (clears task files) |
| `/init` | Initialize ferrus in the current directory |
| `/register` | Register agent configs |
| `/model` | Update the supervisor or executor model override |
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
| `respond_consult` | Consultation | Record the consultation response and let the Executor resume via `/wait_for_consult` |

### Executor
| Tool | From state | Description |
|---|---|---|
| `wait_for_task` | — | Long-poll until Executing or Addressing |
| `check` | Executing, Addressing | Run all configured checks; use it freely during development and again immediately before final `/submit` |
| `consult` | Executing, Addressing | Ask the Supervisor for guidance; moves to Consultation |
| `wait_for_consult` | Consultation | Block until the Supervisor responds; restores previous state |
| `submit` | Executing, Addressing | Run the final review gate and, on success, write submission notes and move to Reviewing |
| `ask_human` | Executing, Addressing, Consultation, Reviewing | Last-resort human fallback. Write question to QUESTION.md; moves to AwaitingHuman. Call `/wait_for_answer` immediately after. |
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
| `ferrus://review` | Supervisor rejection notes (`REVIEW.md`) |
| `ferrus://submission` | Executor submission notes (`SUBMISSION.md`) |
| `ferrus://question` | Pending human question (`QUESTION.md`) |
| `ferrus://answer` | Human answer (`ANSWER.md`) |
| `ferrus://consult_template` | Consultation request template (`CONSULT_TEMPLATE.md`) |
| `ferrus://consult_request` | Pending supervisor consultation request (`CONSULT_REQUEST.md`) |
| `ferrus://consult_response` | Supervisor consultation response (`CONSULT_RESPONSE.md`) |
| `ferrus://state` | Current task state as JSON (`STATE.json`) |

## MCP prompts

| Prompt | Description |
|---|---|
| `executor-context` | State + task + review notes bundled for the Executor |
| `supervisor-review` | State + task + submission notes bundled for the Supervisor |

## ferrus.toml

```toml
[checks]
commands = ["cargo clippy -- -D warnings", "cargo fmt --check", "cargo test"]

[limits]
max_check_retries = 20   # check failures before Failed
max_review_cycles = 3    # reject→fix cycles before Failed
max_feedback_lines = 30  # lines per command shown in /check and /submit output
wait_timeout_secs = 60   # max duration of one wait_* tool call; agents should call again after timeout

[lease]
ttl_secs = 90            # lease validity without renewal
heartbeat_interval_secs = 30  # how often to call /heartbeat

[hq.supervisor]
agent = "claude-code"   # agent for supervisor/reviewer role: claude-code | codex
model = ""              # optional override; empty = agent default

[hq.executor]
agent = "codex"         # agent for executor role: claude-code | codex
model = ""              # optional override; empty = agent default
```

## Runtime files (`.ferrus/`)

| File | Contents |
|---|---|
| `STATE.json` | State, counters, schema version, timestamp, PID |
| `STATE.lock` | Advisory lock file for atomic claiming |
| `TASK.md` | Task description |
| `REVIEW.md` | Rejection notes |
| `SUBMISSION.md` | Submission notes |
| `QUESTION.md` | Pending human question |
| `ANSWER.md` | Human answer |
| `CONSULT_TEMPLATE.md` | Read-only consultation request template |
| `CONSULT_REQUEST.md` | Pending supervisor consultation request |
| `CONSULT_RESPONSE.md` | Supervisor consultation response |
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

    let consult_template_path = dir.join("CONSULT_TEMPLATE.md");
    if !consult_template_path.exists() {
        tokio::fs::write(&consult_template_path, CONSULT_TEMPLATE)
            .await
            .context("Failed to write .ferrus/CONSULT_TEMPLATE.md")?;
        println!("Created .ferrus/CONSULT_TEMPLATE.md");
    }

    let state_path = dir.join("STATE.json");
    if !state_path.exists() {
        store::write_state(&StateData::default())
            .await
            .context("Failed to write .ferrus/STATE.json")?;
        println!("Created .ferrus/STATE.json");
    }

    for filename in [
        "TASK.md",
        "REVIEW.md",
        "SUBMISSION.md",
        "QUESTION.md",
        "ANSWER.md",
        "CONSULT_REQUEST.md",
        "CONSULT_RESPONSE.md",
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
