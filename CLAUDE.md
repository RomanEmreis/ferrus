# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

`ferrus` is a Rust MCP server that coordinates AI agents: a **Supervisor** (plans, reviews) and one or more **Executors** (writes code, fixes issues). State is shared via `.ferrus/` on disk; two separate ferrus processes (one per agent) communicate through that directory.

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
ferrus init [--agents-path <path>]
    # scaffold ferrus.toml, .ferrus/ (incl. STATE.lock), and skill files (default: .agents)

ferrus serve [--role supervisor|executor] [--agent-name <name>] [--agent-index <n>]
    # start the MCP server on stdio
    # --agent-name / --agent-index are baked into the claimed_by field (e.g. "executor:codex:1")
    # defaults: agent-name=unknown, agent-index=0

ferrus register [--supervisor <agent>] [--executor <agent>]
    # write MCP config for claude-code (.mcp.json) or codex (.codex/config.toml)
    # auto-assigns --agent-index; at least one flag required
    # e.g. ferrus register --supervisor claude-code --executor codex
```

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
| `/next_task` | Executing, Addressing | — | Read task + any feedback/review notes |
| `/check` | Executing, Addressing | Checking / Addressing / Failed | Run all configured checks |
| `/submit` | Checking | Reviewing | Write submission notes + signal ready for Supervisor review |

### Shared tools

| Tool | From state | To state | Description |
|---|---|---|---|
| `/heartbeat` | any claimed | — | Renew lease; call every ~30s while working |
| `/ask_human` | Executing, Addressing, Checking, Reviewing | AwaitingHuman (fallback) | Ask the human a question; uses MCP elicitation when supported, otherwise pauses to AwaitingHuman |
| `/answer` | AwaitingHuman | (previous state) | Provide a response when MCP elicitation is unavailable; restores the paused state |
| `/status` | any | — | Print current state + retry counters |
| `/reset` | Failed | Idle | Human escape hatch; clears feedback, review, and submission files |

## MCP Resources

| URI | Contents |
|---|---|
| `ferrus://task` | Current task description (`TASK.md`) |
| `ferrus://feedback` | Check failure summary (`FEEDBACK.md`) |
| `ferrus://review` | Supervisor rejection notes (`REVIEW.md`) |
| `ferrus://submission` | Executor submission notes (`SUBMISSION.md`) |
| `ferrus://question` | Pending human question (`QUESTION.md`) |
| `ferrus://state` | Current task state as JSON (`STATE.json`) |

Resources are read-only. All six are listed via `resources/list` and readable via `resources/read`.

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
             ├─► [FAIL, retries < max] Addressing → /check again (loop)
             ├─► [FAIL, retries ≥ max] Failed
             └─► Reviewing ← /submit
                   ├─► [REJECT] Addressing → /check loop (retries reset)
                   │     └─► [cycles ≥ max] Failed
                   └─► Complete ← /approve
```

Any active state (Executing, Addressing, Checking, Reviewing) can pause to `AwaitingHuman` via `/ask_human` when elicitation is unavailable. `/answer` restores the previous state.

`/reset`: Failed → Idle (human intervention).

## Runtime Files (`.ferrus/`)

| File | Contents |
|---|---|
| `STATE.json` | Current `TaskState`, lease fields (`claimed_by`, `lease_until`, `last_heartbeat`), retry/cycle counters, failure reason, schema version, last-write timestamp and PID |
| `STATE.lock` | Advisory lock file for atomic claiming (do not delete) |
| `TASK.md` | Task description written by Supervisor |
| `FEEDBACK.md` | Short check-failure summary (failed commands, last N lines each, log path) |
| `REVIEW.md` | Supervisor rejection notes |
| `SUBMISSION.md` | Executor's submission notes (summary, verification steps, known limitations) |
| `QUESTION.md` | Question written by `/ask_human` when elicitation is unavailable |
| `ANSWER.md` | Answer written by `/answer` |
| `logs/check_<attempt>_<ts>.txt` | Full stdout + stderr for each check run |

`.ferrus/` is gitignored by `ferrus init`.

## Source Layout

```
src/
  main.rs                    # CLI entry, tracing init
  cli/                       # clap subcommands (init, serve, register)
  config/mod.rs              # Deserialize ferrus.toml (ChecksConfig, LimitsConfig, LeaseConfig)
  state/machine.rs           # TaskState enum + StateData + transition methods + lease helpers
  state/store.rs             # Async read/write of .ferrus/ files; open_lock_file, claim_state
  checks/runner.rs           # Spawn check subprocesses, collect output
  server/mod.rs              # neva App setup; constructs agent_id, wires closures
  server/tools/              # One file per MCP tool
    heartbeat.rs             # /heartbeat — lease renewal
    wait_for_task.rs         # /wait_for_task — atomic claim loop (STATE.lock + fs2)
    wait_for_review.rs       # /wait_for_review — same pattern for Supervisor
```
