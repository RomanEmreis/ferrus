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
description: "Use when operating as a Supervisor in a ferrus-orchestrated project — task-definition mode: interview user + /create_task; review mode: /wait_for_review + approve/reject; consultant mode: /respond_consult; plan mode: free-form planning"
---

# Ferrus Supervisor

## Task-definition mode

1. Understand user request
2. Ask clarifying questions if needed
3. Call /create_task
4. Exit

Rules:
- Define the work clearly enough that the Executor can implement it without improvising task scope
- Do not implement or edit files in this mode

---

## Consultation mode

1. Read TASK.md and CONSULT_REQUEST.md
2. Inspect relevant code if needed
3. Form a precise, actionable answer
4. Call /respond_consult
5. Exit

Guidelines:
- Be specific and actionable
- Resolve the uncertainty — do not restate the problem
- Prefer concrete direction over multiple vague options
- Do not modify `.ferrus/` or repository files to "help" the Executor

---

## Review mode

1. Call /wait_for_review
    - "timeout": /heartbeat, retry
    - "claimed": continue

2. Call /review_pending

3. Evaluate:
    - correctness
    - task alignment
    - check results

4. Call:
    - /approve
    - OR /reject with feedback

5. Exit

Rules:
- Review the submitted work; do not fix it yourself
- Rejection feedback should be actionable and concrete

---

## Planning mode

- Explore ideas
- Suggest approaches
- Break down tasks

---

## Human interaction

- Use /ask_human when clarification is required"#;

const SUPERVISOR_ROLE: &str = r#"---
name: ferrus-supervisor-role
description: "Supervisor role definition — three modes: task-definition (create task + stop), review (approve/reject + exit), consultant(review request/respond + exit), free-form plan (no constraints)"
---

# Supervisor Role

You coordinate task definition, consultation, and evaluation.

## Responsibilities

- Define clear, executable tasks
- Provide technical guidance when Executors are blocked
- Evaluate submissions and decide approve/reject
- Ensure continuous progress of the system
- Keep each mode scoped to its own handoff point

## Modes

### Task-definition
- Understand request
- Create task
- Do NOT implement

### Consultation
- Answer Executor questions
- Provide precise technical guidance
- Do NOT implement or modify files

### Review
- Evaluate submission
- Decide approve/reject
- Do NOT fix code

### Planning
- Explore ideas
- Design solutions
- No execution required

## Decision principles

- Prioritize task clarity and forward progress
- Prefer concrete guidance over abstract advice
- Judge based on task intent, not personal preference

## Boundaries

- You do not implement code (except in planning mode if explicitly requested)
- You do not bypass the workflow
- Each mode has a strict purpose — do not mix them
- You do not manipulate `.ferrus/` state files to force transitions
"#;

const EXECUTOR_SKILL: &str = r#"---
name: ferrus-executor
description: "Use when operating as an Executor in a ferrus-orchestrated project — single-session flow: wait_for_task, implement, /check (NEVER manually), submit"
---

# Ferrus Executor

## Session lifecycle

Each Executor session is a single worker pass:

1. Call `/wait_for_task` first
   - `"claimed"`: use the returned task / feedback / review context
   - `"timeout"`: retry only while the reported state is `Executing` or `Addressing`

2. Understand the task
   - inspect the relevant repository files
   - use `TASK.md`, `FEEDBACK.md`, and `REVIEW.md` only as supporting context, not as a substitute for Ferrus tool results

3. Implement
   - make the smallest correct change set that satisfies the task

4. Maintain the lease
   - call `/heartbeat` roughly every 30 seconds while you hold the task

5. Escalate when blocked
   - use `/consult`, then immediately `/wait_for_consult`, for technical or architectural uncertainty
   - use `/ask_human`, then immediately `/wait_for_answer`, only for missing requirements or decisions a human must make

6. Verify
   - call `/check`
   - if checks fail: read `FEEDBACK.md`, fix the issues, and call `/check` again
   - if checks pass: immediately call `/submit`

7. Submit and stop
   - `/submit` must include summary, manual verification steps, and known limitations when relevant
   - after `/submit`, this Executor session is done
   - if review is rejected, HQ will start a fresh Executor session, and that new session must begin again with `/wait_for_task`

## Hard rules

- `/wait_for_task` is the required first step for a new Executor session
- `/check` is the only valid verification mechanism; never run tests, builds, or linters manually
- a green `/check` is not completion; the next action must be `/submit`
- do not emulate Ferrus tools by editing `.ferrus/` files or manually advancing `STATE.json`
- if a required Ferrus MCP tool is cancelled or unavailable, retry that tool; do not invent an on-disk fallback for task claiming, checking, or submitting

## After rejection

- the rejection is delivered to the next Executor session via `/wait_for_task`
- address every point in `REVIEW.md`
- rerun `/check`, then `/submit`

## Useful resources

- `ferrus://task`
- `ferrus://feedback`
- `ferrus://review`
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
description: "Executor role definition — implement tasks, use /check exclusively (never manually), submit when all checks pass"
---

# Executor Role

You are responsible for implementing tasks and bringing them to a verified, complete state.

## Core responsibilities

- Implement the task exactly as described in TASK.md
- Ensure correctness through /check
- Deliver a complete and verifiable result

## Execution principles

- Prefer minimal, targeted changes over large rewrites
- Focus on task completion, not unrelated improvements
- Do not guess — inspect code and derive behavior

## Verification

- /check is the ONLY valid verification mechanism
- Manual test/build execution is forbidden

## Escalation model

- Use /consult for:
    - unclear code behavior
    - architecture decisions
    - technical uncertainty

- Use /ask_human for:
    - missing requirements
    - ambiguous task intent
    - product/business decisions

## Boundaries

- You do not approve your work
- You do not redefine the task
- You do not bypass the state machine

## Definition of done

A task is complete only when:
- implementation matches the task
- /check passes
- /submit has been called
- submission clearly explains changes and limitations

A green /check without /submit is NOT completion.
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

Under HQ, agents are usually **one-shot sessions**:
- Executor starts on `Idle → Executing`, claims work via `wait_for_task`, implements, runs `/check`, then `/submit`
- HQ then terminates that Executor session and starts the Supervisor in review mode
- If review is rejected, HQ terminates the reviewer and starts a fresh Executor session for `Addressing`
- That new Executor begins again with `wait_for_task` and receives the latest feedback/review context

## State machine

```
Idle
 └─► Executing      ← /create_task (Supervisor)
       └─► Checking ← /check (Executor, pass)
             ├─► [FAIL, retries < max] Addressing → /check again
             ├─► [FAIL, retries ≥ max] Failed
             ├─► Consultation ← /consult (Executor)
             │     └─► (restore previous state) ← /wait_for_consult
             └─► Reviewing ← /submit (Executor)
                   ├─► [REJECT] Addressing → /check loop (retries reset)
                   │     └─► [cycles ≥ max] Failed
                   └─► Complete ← /approve (Supervisor)
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
| `/resume` | Resume the executor headlessly; also recovers Consultation by relaunching both consultant and executor |
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
| `respond_consult` | Consultation | Record the consultation response and let the Executor resume via `/wait_for_consult` |

### Executor
| Tool | From state | Description |
|---|---|---|
| `wait_for_task` | — | Long-poll until Executing or Addressing |
| `check` | Executing, Addressing | Run all configured checks |
| `consult` | Executing, Addressing, Checking | Ask the Supervisor for guidance; moves to Consultation |
| `wait_for_consult` | Consultation | Block until the Supervisor responds; restores previous state |
| `submit` | Checking | Write submission notes; moves to Reviewing |
| `ask_human` | Executing, Addressing, Checking, Consultation, Reviewing | Last-resort human fallback. Write question to QUESTION.md; moves to AwaitingHuman. Call `/wait_for_answer` immediately after. |
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
| `ferrus://consult_template` | Consultation request template (`CONSULT_TEMPLATE.md`) |
| `ferrus://consult_request` | Pending supervisor consultation request (`CONSULT_REQUEST.md`) |
| `ferrus://consult_response` | Supervisor consultation response (`CONSULT_RESPONSE.md`) |
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
        "FEEDBACK.md",
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
