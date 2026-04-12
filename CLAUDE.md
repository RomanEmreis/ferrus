# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

`ferrus` is a Rust AI agent orchestrator for software projects. It drives a **Supervisor–Executor** workflow: the Supervisor plans tasks and reviews submissions, the Executor implements and checks its own work. State is shared via `.ferrus/` on disk; agents coordinate through that directory using MCP as the tool transport.

Licensed under Apache 2.0.

## Commands

```sh
cargo build            # compile
cargo build --release  # optimized build
cargo test             # run all tests
cargo test <name>      # run a single test by name
cargo clippy           # lint
cargo fmt              # format
cargo check            # fast type-check without producing a binary
```

## CLI

```sh
ferrus                    # enter HQ (interactive orchestration shell)

ferrus init [--agents-path <path>]
    # scaffold ferrus.toml, .ferrus/ (incl. STATE.lock, agents.json), and skill files (default: .agents)

ferrus serve [--role supervisor|executor] [--agent-name <name>] [--agent-index <n>]
    # start the MCP server on stdio
    # --agent-name / --agent-index are baked into the claimed_by field (e.g. "executor:codex:1")
    # defaults: agent-name=unknown, agent-index=0

ferrus register [--supervisor <agent>] [--supervisor-model <model>] [--executor <agent>] [--executor-model <model>]
    # write MCP config for claude-code (.mcp.json) or codex (.codex/config.toml)
    # at least one flag required
    # e.g. ferrus register --supervisor claude-code --executor codex
```

### HQ shell commands

| Command | Description |
|---|---|
| `/plan` | Free-form planning session with the supervisor (no task created, no state requirement) |
| `/task` | Define a task with the supervisor, then run the executor→review loop automatically |
| `/supervisor` | Open an interactive supervisor session (no initial prompt, no state requirement) |
| `/executor` | Open an interactive executor session (no initial prompt, no state requirement) |
| `/resume` | Manually resume the executor headlessly; also recovers Consultation by relaunching both consultant and executor |
| `/review` | Manually spawn supervisor in review mode (escape hatch when automatic spawning failed) |
| `/status` | Show task state, agent list, and session log paths |
| `/attach <name>` | Show log path for a running headless agent (both supervisor and executor run headlessly) |
| `/stop` | Stop all running agent sessions (prompts for confirmation) |
| `/reset` | Reset state to Idle and clear task files (prompts for confirmation) |
| `/init [--agents-path]` | Initialize ferrus in the current directory |
| `/register` | Register agent configs (same as `ferrus register`) |
| `/model` | Update the supervisor or executor model override |
| `/help` | List all HQ commands |
| `/quit` | Exit HQ |

**Quit HQ:** Press **Ctrl+C** twice within 2 seconds to exit. The first press shows a confirmation prompt in the status line; the second confirms and exits.

Set `RUST_LOG=ferrus=debug` (or `info`/`warn`) to control log verbosity.
Logs go to **stderr** so they don't interfere with the stdio MCP stream.

## ferrus.toml

```toml
[checks]
commands = [
    "cargo clippy -- -D warnings",
    "cargo fmt --check",
    "cargo test",
]

[limits]
max_check_retries = 5    # consecutive check failures before state → Failed
max_review_cycles = 3    # reject→fix cycles before state → Failed
max_feedback_lines = 30  # trailing lines per failing command shown in FEEDBACK.md
wait_timeout_secs = 3600 # /wait_for_task and /wait_for_review poll timeout

[lease]
ttl_secs = 90            # how long a claimed lease is valid without renewal
heartbeat_interval_secs = 30  # how often agents should call /heartbeat

[hq.supervisor]
agent = "claude-code"  # agent for supervisor/reviewer role: claude-code | codex
model = ""             # optional override; empty = agent default

[hq.executor]
agent = "codex"        # agent for executor role: claude-code | codex
model = ""             # optional override; empty = agent default
```

## Skill Files

`ferrus init` creates three skill files under `<agents-path>/skills/` (default `.agents/skills/`):

| File | Purpose |
|---|---|
| `ferrus/SKILL.md` | General overview: full tool reference, state machine, resources, prompts, config |
| `ferrus-supervisor/SKILL.md` | Supervisor how-to: step-by-step workflow |
| `ferrus-supervisor/ROLE.md` | Supervisor role definition and boundaries |
| `ferrus-executor/SKILL.md` | Executor how-to: autonomous loop |
| `ferrus-executor/ROLE.md` | Executor role definition and boundaries |

## MCP Tool Reference

### Supervisor tools

| Tool | From state | To state | Description |
|---|---|---|---|
| `/create_task` | Idle | Executing | Write task description; Executor picks it up |
| `/wait_for_review` | Reviewing | — | Long-poll until state is Reviewing, then return submission context |
| `/review_pending` | Reviewing | — | Read task + context for review |
| `/approve` | Reviewing | Complete | Accept the submission |
| `/reject` | Reviewing | Addressing | Reject with notes; resets Executor retry counter |

### Executor tools

| Tool | From state | To state | Description |
|---|---|---|---|
| `/wait_for_task` | Executing, Addressing | — | Long-poll until a task is ready, then return full task context |
| `/check` | Executing, Addressing | Checking / Addressing / Failed | Run all configured checks |
| `/consult` | Executing, Addressing, Checking | Consultation | Ask the Supervisor for guidance; Executor should prefer this before `/ask_human` |
| `/wait_for_consult` | Consultation | (previous state) | Block until the Supervisor responds; restores paused state and returns the answer |
| `/submit` | Checking | Reviewing | Write submission notes + signal ready for Supervisor review |
| `/wait_for_answer` | AwaitingHuman | (previous state) | Block until the human answers; restores paused state and returns the answer |

### Shared tools

| Tool | From state | To state | Description |
|---|---|---|---|
| `/ask_human` | Executing, Addressing, Checking, Consultation, Reviewing | AwaitingHuman | Last-resort human fallback. Write question to QUESTION.md; agent must immediately call `/wait_for_answer` (executor) or wait for HQ to answer |
| `/respond_consult` | Consultation | — | Record the Supervisor consultation response in `CONSULT_RESPONSE.md` |
| `/answer` | AwaitingHuman | (previous state) | Provide answer to a pending question; restores previous state |
| `/heartbeat` | any claimed | — | Renew lease; call every ~30s while working |
| `/status` | any | — | Print current state + retry counters |
| `/reset` | Failed | Idle | MCP escape hatch; clears feedback, review, and submission files. HQ `/reset` command works from any state. |

## MCP Resources

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

Resources are read-only. All nine are listed via `resources/list` and readable via `resources/read`.

## MCP Prompts

| Prompt | Description |
|---|---|
| `executor-context` | State + task + feedback + review notes bundled for the Executor |
| `supervisor-review` | State + task + submission notes bundled for the Supervisor |

## State Machine

```
Idle
 └─► Executing      ← /create_task
       └─► Checking ← /check (pass)
             ├─► Consultation ← /consult
             │     └─► (restore previous state) ← /wait_for_consult
             ├─► [FAIL, retries < max] Addressing → /check again (loop)
             ├─► [FAIL, retries ≥ max] Failed
             └─► Reviewing ← /submit
                   ├─► [REJECT] Addressing → /check loop (retries reset)
                   │     └─► [cycles ≥ max] Failed
                   └─► Complete ← /approve
```

Any active Executor work state (Executing, Addressing, Checking) can pause to `Consultation` via `/consult`. HQ spawns the configured Supervisor in consultation mode, and the executor immediately calls `/wait_for_consult` to block until the Supervisor answers via `/respond_consult`, which writes `CONSULT_RESPONSE.md`. The previous state is then restored.

Any active state, including `Consultation`, can pause to `AwaitingHuman` via `/ask_human`. The executor should prefer `/consult` first and use `/ask_human` only as a last resort. The agent immediately calls `/wait_for_answer` to block until the human responds. The human types their answer in the HQ terminal (raw text, no slash prefix). `/wait_for_answer` restores the previous state and returns the answer.

- `/task` from `Complete` → silently resets to Idle and starts the next task.
- HQ `/reset` command: works from any state; prompts for confirmation if an agent is actively working. The MCP `/reset` tool is only valid from `Failed`.

## Runtime Files (`.ferrus/`)

| File | Contents |
|---|---|
| `STATE.json` | Current `TaskState`, lease fields (`claimed_by`, `lease_until`, `last_heartbeat`), retry/cycle counters, failure reason, schema version, last-write timestamp and PID |
| `STATE.lock` | Advisory lock file for atomic claiming (do not delete) |
| `TASK.md` | Task description written by Supervisor |
| `FEEDBACK.md` | Short check-failure summary (failed commands, last N lines each, log path) |
| `REVIEW.md` | Supervisor rejection notes |
| `SUBMISSION.md` | Executor's submission notes (summary, verification steps, known limitations) |
| `QUESTION.md` | Question written by `/ask_human` |
| `ANSWER.md` | Answer written by `/answer` |
| `CONSULT_TEMPLATE.md` | Read-only consultation request template |
| `CONSULT_REQUEST.md` | Question written by `/consult` |
| `CONSULT_RESPONSE.md` | Answer written by the consultation Supervisor |
| `logs/check_<attempt>_<ts>.txt` | Full stdout + stderr for each check run |

`.ferrus/` is gitignored by `ferrus init`.

## Source Layout

```
src/
  main.rs                    # CLI entry, tracing init, HQ logger
  cli/                       # clap subcommands (init, serve, register)
  config/mod.rs              # Deserialize ferrus.toml (ChecksConfig, LimitsConfig, LeaseConfig, HqConfig)
  state/machine.rs           # TaskState enum + StateData + transition methods + lease helpers
  state/store.rs             # Async read/write of .ferrus/ files; open_lock_file, claim_state
  state/agents.rs            # AgentEntry, AgentsRegistry — .ferrus/agents.json lifecycle tracking
  checks/runner.rs           # Spawn check subprocesses, collect output
  hq/mod.rs                  # HQ entry point; HqContext; tokio::select! loop; transition_action
  hq/state_watcher.rs        # Background task: polls STATE.json every 250ms, sends on watch channel
  hq/tui.rs                  # Terminal UI (crossterm): App event loop, UiMessage, StatusSnapshot; autocomplete, command history, status line, confirmation dialogs
  hq/commands.rs             # ShellCommand enum, parse_command() via clap + shlex
  hq/display.rs              # Display wrapper: sends UiMessage to TUI channel (info, error, transition, status, suspend, resume, confirm)
  hq/agent_manager.rs        # agent spawn helpers (headless for both executor and reviewer); HeadlessHandle; agents.json updates
  server/mod.rs              # neva App setup; constructs agent_id, wires closures
  server/tools/              # One file per MCP tool
    heartbeat.rs             # /heartbeat — lease renewal
    wait_for_task.rs         # /wait_for_task — atomic claim loop (STATE.lock + fs2)
    wait_for_review.rs       # /wait_for_review — same pattern for Supervisor
    consult.rs               # /consult — writes CONSULT_REQUEST.md and transitions to Consultation
    respond_consult.rs       # /respond_consult — records the supervisor consultation response
    wait_for_consult.rs      # /wait_for_consult — polls CONSULT_RESPONSE.md, restores state, returns answer
    ask_human.rs             # /ask_human — writes QUESTION.md, transitions to AwaitingHuman
    wait_for_answer.rs       # /wait_for_answer — polls ANSWER.md, restores state, returns answer
```

<!-- ferrus-supervisor-instructions -->
## Ferrus Supervisor

This repository is orchestrated by Ferrus HQ.

Your initial prompt tells you which mode you are in. Match it exactly.

**Task-definition mode** ("You are a Ferrus Supervisor in TASK DEFINITION mode"): Interview the user, draft the exact task text, show that draft to the user, gather feedback, and call `/create_task` only after the user explicitly approves the task text. HQ terminates this session once `/create_task` succeeds.

MUST NOT in task-definition mode:
- MUST NOT write, edit, or create any files
- MUST NOT run commands or implement code
- MUST NOT explore the codebase to design a solution yourself
- MUST NOT call `/create_task` before the user has explicitly approved the task text

**Review mode** ("You are a Ferrus Supervisor in REVIEW mode"): Call `/wait_for_review`, then `/review_pending`, then `/approve` or `/reject`. After deciding, **exit**.

MUST NOT in review mode:
- MUST NOT implement fixes or changes yourself
- MUST NOT ask the Executor to re-verify

**Free-form plan mode** ("You are a Ferrus Supervisor in free-form planning mode"): No hard constraints. Explore, discuss, write plans. `/create_task` is available but not required.

**Consultation mode** ("You are a Ferrus Supervisor in CONSULTATION mode"): Read `TASK.md` + `CONSULT_REQUEST.md`, investigate read-only, call `/respond_consult`, then exit. You may use `/ask_human` if the answer cannot be determined from the repository.

See `.agents/skills/ferrus-supervisor/SKILL.md` for the full workflow.

<!-- ferrus-executor-instructions -->
## Ferrus Executor

This repository is orchestrated by Ferrus HQ.

When spawned by `ferrus` HQ, your initial prompt will tell you what to do.

If started manually: call MCP tool `/wait_for_task` as your first action.

**HARD RULE — no exceptions: NEVER run check commands manually** (`cargo test`, `cargo clippy`, `cargo build`, `npm test`, `make`, `pytest`, or any build/test/lint command). Always use the `/check` MCP tool — it records results, updates state, and handles retry counting. Running checks manually bypasses the state machine entirely: retry counters won't increment, FEEDBACK.md won't be updated, and state transitions won't fire.

Prefer `/consult` over `/ask_human`. Use `/ask_human` only as a last resort if consultation still leaves you blocked.

Full workflow: `.agents/skills/ferrus-executor/SKILL.md`
