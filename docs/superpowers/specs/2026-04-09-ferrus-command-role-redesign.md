# Ferrus Command & Role Redesign

**Date:** 2026-04-09  
**Status:** Approved

## Problem

Two observed failures in the Supervisor–Executor workflow:

1. **Supervisor in plan mode implements instead of task-creating.** Claude Code reads the codebase, designs an implementation, gets user approval, then writes code itself — leaving nothing for the Executor to do.

2. **Executor runs checks manually.** Codex runs `cargo clippy`, `cargo test`, etc. directly in addition to calling `/check`, bypassing state tracking, retry counting, and FEEDBACK.md generation.

Root causes:
- The initial prompt said "You are in planning mode" — a naming collision with Claude Code's own internal `--permission-mode plan`, which reinforces "explore and implement" behavior.
- `--permission-mode plan` was passed to the supervisor launch, further signaling Claude Code to plan-then-implement.
- Critical constraints ("do not implement", "never run checks manually") were buried in `## Notes` sections rather than leading the skill files.
- Initial prompts were too short and contained no explicit prohibitions.

## Design

### Commands

| Command | State req. | Mode | Spawns | Drives loop? |
|---|---|---|---|---|
| `/plan` | None | Interactive (TUI) | Supervisor | No |
| `/task` | Idle or Complete | Interactive (TUI) | Supervisor → Executor + reviewer loop | Yes |
| `/supervisor` | None | Interactive (TUI) | Supervisor, no initial prompt | No |
| `/executor` | None | Interactive (TUI) | Executor, no initial prompt | No |
| `/resume` | — | Headless | Executor (escape hatch, was `/execute`) | No |
| `/review` | Reviewing | Headless | Supervisor reviewer (escape hatch) | No |

**`/plan`** is now a free-form planning session. No `--permission-mode plan`, no state requirement, no expectation that `/create_task` will be called. The supervisor can explore, discuss, and plan — whatever the user needs.

**`/task`** is the strict workflow trigger. No `--permission-mode plan` (dropped entirely — it was reinforcing the wrong behavior). The supervisor's only job is to interview the user and call `/create_task`. After that, HQ auto-spawns the executor and drives the full loop.

**`/supervisor`** and **`/executor`** are plain interactive sessions — no initial prompt, no special flags. Useful for manual intervention, debugging, or ad-hoc MCP tool calls.

**`/resume`** replaces `/execute` (renamed for clarity).

### Initial Prompts (`src/hq/agent_manager.rs`)

All prompts get an explicit `HARD RULES` block. Critical constraints belong in the initial prompt itself, not just the skill file.

**`SUPERVISOR_TASK_PROMPT`** (new, for `/task`):
```
You are a Ferrus Supervisor in TASK DEFINITION mode.

YOUR ONLY JOB: Interview the user about what needs to be done, then call /create_task
with a complete task description. The HQ terminates this session automatically once
/create_task succeeds and hands off to the Executor.

HARD RULES — no exceptions:
  - DO NOT write, edit, or create any files
  - DO NOT run any commands or implement any code
  - DO NOT explore the codebase to design a solution yourself
  - DO NOT ask the Executor to verify anything — it does not exist yet
  - Call /create_task as soon as you have enough information; never implement first

After /create_task succeeds you are done. The HQ handles everything else.
See .agents/skills/ferrus-supervisor/SKILL.md for the full workflow.
```

**`SUPERVISOR_PLAN_PROMPT`** (rewritten, for `/plan`):
```
You are a Ferrus Supervisor in free-form planning mode.

Explore the codebase, discuss ideas, and help the user plan. You are NOT required to
call /create_task — this is a freeform planning conversation. Use ferrus MCP tools
(e.g. /status) as needed. There are no hard constraints on what you may do.

See .agents/skills/ferrus-supervisor/SKILL.md for context on the ferrus workflow.
```

**`REVIEWER_PROMPT`** (strengthened):
```
You are a Ferrus Supervisor in REVIEW mode.

Call /wait_for_review, then /review_pending to read the submission. Make one decision:
/approve or /reject with specific feedback. Then exit — the HQ handles the next cycle.

HARD RULES — no exceptions:
  - DO NOT implement any fixes or changes yourself
  - DO NOT ask the Executor to re-verify — the submission is already in; your job is to judge it
  - Make exactly one decision: /approve or /reject

See .agents/skills/ferrus-supervisor/SKILL.md for the full workflow.
```

**`EXECUTOR_PROMPT`** (strengthened):
```
You are a Ferrus Executor. Call /wait_for_task, implement the assigned task, then submit.

HARD RULES — no exceptions:
  - NEVER run cargo, npm, make, pytest, or any check/build/test command manually
  - ALWAYS use /check for all verification — it records results, updates state, and
    handles retry counting; running checks manually bypasses the state machine entirely
  - Do not call /submit until /check returns a passing result

See .agents/skills/ferrus-executor/SKILL.md for the full workflow.
```

**`EXECUTOR_RESUME_PROMPT`** (strengthened):
```
You are a Ferrus Executor being relaunched after a human answered your question.
The answer is in .ferrus/ANSWER.md — read it, then continue your work.
Call /wait_for_task and resume the assigned task from where you left off.

HARD RULES — no exceptions:
  - NEVER run cargo, npm, make, pytest, or any check/build/test command manually
  - ALWAYS use /check for all verification
  - Do not call /submit until /check returns a passing result

See .agents/skills/ferrus-executor/SKILL.md for the full workflow.
```

**`/supervisor` and `/executor`:** No initial prompt — spawn the agent binary with stdin/stdout inherited only.

### Skill Files (`.agents/skills/` and `src/cli/commands/init.rs` templates)

Both the deployed files and the Rust string templates in `init.rs` must be updated together.

**Structure rule:** `## Hard Rules` section appears at the top of every skill file, before any workflow steps.

#### `ferrus-supervisor/SKILL.md`

```markdown
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
2. Call /create_task with a complete Markdown description
3. Done — HQ terminates this session and spawns the Executor

You do NOT write files. You do NOT implement code. You do NOT explore the codebase
to design a solution. Your sole output is the task description in /create_task.

## Review mode

Initial prompt: "You are a Ferrus Supervisor in REVIEW mode."

1. Call /wait_for_review (on "timeout": /heartbeat, retry)
2. Call /review_pending — reads task + submission
3. Call /heartbeat every ~30s while reviewing
4. Call /approve or /reject with specific feedback
5. Exit — HQ handles the next cycle

You do NOT implement fixes. You do NOT ask the Executor to re-verify.
One decision: /approve or /reject. Then exit.

## Free-form plan mode

Initial prompt: "You are a Ferrus Supervisor in free-form planning mode."

No hard constraints. Explore, discuss, write plans. /create_task is available but not required.
```

#### `ferrus-supervisor/ROLE.md`

```markdown
# Supervisor Role

## Hard Rules — read this first

Task-definition mode: You do NOT write files, implement code, or run commands.
Your only job is to call /create_task with a task description, then stop.

Review mode: You do NOT implement fixes. You do NOT ask the Executor to re-verify.
You make one decision — /approve or /reject — then exit.

## Three modes

Task-definition ("TASK DEFINITION mode"): interview → /create_task → done
Review ("REVIEW mode"): /wait_for_review → read context → approve or reject → exit
Free-form plan ("free-form planning mode"): no constraints

## Responsibilities

- Write tasks with clear acceptance criteria and enough context for autonomous implementation
- Review submissions and make a single approve/reject decision
- Reject only on concrete problems; do not block on preferences not stated in the task
```

#### `ferrus-executor/SKILL.md`

```markdown
# Ferrus Executor

## Hard Rules — read this first

NEVER run check commands manually: no cargo test, cargo clippy, cargo fmt,
npm test, make, pytest, or any equivalent. If you do, results are not recorded,
state is not updated, FEEDBACK.md is not written — the workflow breaks.

ALWAYS use /check. It is the only correct verification path.
Do not call /submit until /check returns a passing result.

## Autonomous loop

1. /wait_for_task — on "timeout": /heartbeat, retry; on "claimed": read task/feedback/review
2. Implement the required changes
3. /heartbeat every ~30 seconds while working
4. /check — read FEEDBACK.md for details, fix failures, repeat until all pass
5. /submit — summary, manual verification steps, known limitations
6. Return to step 1

## When re-addressing after rejection

Read REVIEW.md. Address every point before calling /check again.

## Asking the human

1. /ask_human with your question
2. Immediately /wait_for_answer — do not call anything else in between
   - "answered": use the answer and continue
   - "timeout": call /wait_for_answer again

You run headlessly — no interactive terminal. All human interaction via /ask_human + /wait_for_answer.
```

#### `ferrus-executor/ROLE.md`

```markdown
# Executor Role

## Hard Rules — read this first

NEVER run check commands manually. ALWAYS use /check.
Do not call /submit until /check returns a passing result.

Running checks manually bypasses state tracking entirely — retry counters,
FEEDBACK.md, and state transitions all depend on /check being the sole verification path.

## Responsibilities

- Implement tasks faithfully as described in TASK.md
- Use /check exclusively for all verification
- Submit with a complete summary, verification steps, and known limitations

## Boundaries

- You do not approve your own work — only the Supervisor can
- You do not run check commands manually
- You do not ignore parts of the task description
```

#### `ferrus/SKILL.md` — HQ command table update

```
| /plan         | Free-form planning session with the supervisor (no task created)        |
| /task         | Define a task with the supervisor, then run executor→review loop        |
| /supervisor   | Open an interactive supervisor session (no initial prompt)              |
| /executor     | Open an interactive executor session (no initial prompt)                |
| /resume       | Resume the executor headlessly (escape hatch, was /execute)             |
| /review       | Spawn supervisor in review mode (escape hatch)                          |
```

### CLAUDE.md and AGENTS.md

Both files get updated Supervisor and Executor sections:
- Rename all references to "planning mode" → "task-definition mode" (for the `/task` command)
- Add explicit MUST NOT rules to the Supervisor section
- Add explicit NEVER-run-checks-manually rule to the Executor section
- Update command names (`/plan`, `/task`, `/resume`)

### Code Changes (`src/`)

**`src/hq/commands.rs`:** Add `Task`, `Supervisor`, `Executor` variants; rename `Execute` → `Resume`.

**`src/hq/mod.rs`:**
- Add `plan()`, `task()`, `supervisor_interactive()`, `executor_interactive()` methods
- Rename `execute()` → `resume()`
- `/plan`: spawn supervisor interactively, no state requirement, no `plan_mode_args`, use `SUPERVISOR_PLAN_PROMPT`
- `/task`: current `/plan` behavior minus `plan_mode_args`, using `SUPERVISOR_TASK_PROMPT`; requires Idle or Complete
- `/supervisor` and `/executor`: spawn agent interactively with no initial prompt, no state check
- Update dispatch table and `/help` text

**`src/hq/agent_manager.rs`:** Add `SUPERVISOR_TASK_PROMPT`; rewrite all prompt constants; add `supervisor_task_prompt()` accessor; no behavior changes to spawning helpers.

**`src/cli/commands/init.rs`:** Update all five skill file string constants to match the new content above.
