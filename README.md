# ferrus

An MCP server that coordinates AI agents: a **Supervisor** that plans and reviews, and one or more **Executors** that write code and fix issues. State is shared via `.ferrus/` on disk; each agent runs its own `ferrus` process over stdio and communicates through that directory.

Licensed under Apache 2.0.

---

## How it works

```
  Supervisor (Claude Code)          Executor (Codex / any agent)
        │                                       │
        │ spawns via stdio                      │ spawns via stdio
        ▼                                       ▼
  ferrus --role supervisor            ferrus --role executor
  ┌─────────────────────┐            ┌─────────────────────┐
  │ create_task         │            │ wait_for_task       │
  │ wait_for_review     │            │ next_task           │
  │ review_pending      │            │ check               │
  │ approve / reject    │            │ submit              │
  │ ask_human / answer  │            │ ask_human / answer  │
  │ status / reset      │            │ status / reset      │
  └──────────┬──────────┘            └──────────┬──────────┘
             └──────────────┬────────────────────┘
                            │ shared via filesystem
                     ┌──────────────┐
                     │  .ferrus/    │
                     │  STATE.json  │
                     │  TASK.md     │
                     │  FEEDBACK.md │
                     │  REVIEW.md   │
                     │  SUBMISSION.md│
                     │  QUESTION.md │
                     │  ANSWER.md   │
                     │  logs/       │
                     └──────────────┘
```

Agents run autonomously using `/wait_for_task` and `/wait_for_review` to long-poll for work. When a question requires human input, `/ask_human` uses MCP elicitation; if the client doesn't support it, state pauses to `AwaitingHuman` and the human calls `/answer` to resume.

---

## State machine

```
Idle
 └─► Executing      ← /create_task (Supervisor)
       └─► Checking ← /check (Executor, pass)
             ├─► [FAIL, retries < max] Addressing → /check again
             ├─► [FAIL, retries ≥ max] Failed
             └─► Reviewing ← /submit (Executor)
                   ├─► [REJECT] Addressing → check loop (retries reset)
                   │     └─► [cycles ≥ max] Failed
                   └─► Complete ← /approve (Supervisor)
```

Any active state can pause to `AwaitingHuman` via `/ask_human` (elicitation fallback). `/answer` restores the previous state.

`/reset` moves `Failed → Idle` (human intervention).

---

## Installation

```sh
cargo install --path .
```

---

## Quick start

```sh
ferrus init                                          # scaffold ferrus.toml + .ferrus/
ferrus register --supervisor claude-code --executor codex  # configure agents
ferrus                                               # enter HQ
```

### HQ commands

| Command | Description |
|---|---|
| `/plan` | Spawn supervisor to plan, then run executor→review loop automatically |
| `/status` | Show task state and agent list |
| `/attach <role>` | Attach terminal to a running agent — Ctrl-B d to detach (Phase B) |
| `/quit` | Exit HQ |

### How it works

```
ferrus> /plan
  └─ supervisor spawns → user plans with it → supervisor calls /create_task
       └─ executor spawns → implements → /check → /submit
            └─ reviewer spawns → /approve or /reject
                 ├─ approved → Complete
                 └─ rejected → executor re-spawns with feedback
```

Agents are **stateless** — context lives in `.ferrus/*.md`.
Each spawn receives a short bootstrap prompt referencing those files.

---

## Commands

### `ferrus init [--agents-path <path>]`

Scaffolds ferrus in the current project (default `--agents-path .agents`):

- Creates `ferrus.toml` with default check commands and limits
- Creates `.ferrus/` with `STATE.json`, `TASK.md`, `FEEDBACK.md`, `REVIEW.md`, `SUBMISSION.md`, `QUESTION.md`, `ANSWER.md`, and `logs/`
- Creates skill files:
  - `<agents-path>/skills/ferrus/SKILL.md` — general ferrus overview (tools, resources, prompts, config)
  - `<agents-path>/skills/ferrus-supervisor/SKILL.md` + `ROLE.md` — Supervisor how-to and role definition
  - `<agents-path>/skills/ferrus-executor/SKILL.md` + `ROLE.md` — Executor how-to and role definition
- Adds `.ferrus/` to `.gitignore`

### `ferrus serve [--role supervisor|executor]`

Starts the MCP server on stdio. Pass `--role` to expose only the tools relevant to that agent:

| `--role` | Tools exposed |
|---|---|
| `supervisor` | `create_task`, `wait_for_review`, `review_pending`, `approve`, `reject`, `ask_human`, `answer`, `status`, `reset` |
| `executor` | `wait_for_task`, `next_task`, `check`, `submit`, `ask_human`, `answer`, `status`, `reset` |
| *(omitted)* | All 11 tools |

Set `RUST_LOG=ferrus=debug` (or `info`/`warn`) to control log verbosity. Logs go to stderr so they don't interfere with the stdio MCP stream.

### `ferrus register --supervisor <agent> --executor <agent>`

Writes MCP config files so agents can launch ferrus automatically. Supported agents:

| Agent | Config file written |
|---|---|
| `claude-code` | `.mcp.json` (under `mcpServers`) |
| `codex` | `.codex/config.toml` (under `mcp_servers`) |

Both supervisor and executor entries are written in the same file if both roles use the same agent. Existing config files are read and merged — no entries are overwritten unless they already exist under the same key.

Example: `ferrus register --supervisor claude-code --executor codex` writes:

`.mcp.json`:
```json
{
  "mcpServers": {
    "ferrus-supervisor": {
      "command": "ferrus",
      "args": ["serve", "--role", "supervisor"]
    }
  }
}
```

`.codex/config.toml`:
```toml
[mcp_servers.ferrus-executor]
command = "ferrus"
args = ["serve", "--role", "executor"]
```

---

## MCP tool reference

### Supervisor tools

| Tool | From state | To state | Description |
|---|---|---|---|
| `create_task` | Idle | Executing | Write task description; Executor picks it up |
| `wait_for_review` | — | — | Long-poll until state is Reviewing, then return submission context |
| `review_pending` | Reviewing | — | Read task + context for review |
| `approve` | Reviewing | Complete | Accept the submission |
| `reject` | Reviewing | Addressing | Reject with notes; resets Executor retry counter |

### Executor tools

| Tool | From state | To state | Description |
|---|---|---|---|
| `wait_for_task` | — | — | Long-poll until a task is ready, then return full task context |
| `next_task` | Executing, Addressing | — | Read task + any feedback/review notes |
| `check` | Executing, Addressing | Checking / Addressing / Failed | Run all configured checks |
| `submit` | Checking | Reviewing | Write submission notes + signal ready for Supervisor review |

### Shared tools

| Tool | From state | To state | Description |
|---|---|---|---|
| `ask_human` | Executing, Addressing, Checking, Reviewing | AwaitingHuman (fallback) | Ask the human a question; uses MCP elicitation when supported, otherwise pauses to AwaitingHuman |
| `answer` | AwaitingHuman | (previous state) | Provide a response when MCP elicitation is unavailable; restores the paused state |
| `status` | any | — | Print current state + retry counters |
| `reset` | Failed | Idle | Human escape hatch; clears feedback, review, and submission files |

### MCP resources

Runtime files are exposed as MCP resources for agents that want to pull them on demand without a tool call:

| URI | Contents |
|---|---|
| `ferrus://task` | Current task description (`TASK.md`) |
| `ferrus://feedback` | Check failure summary (`FEEDBACK.md`) |
| `ferrus://review` | Supervisor rejection notes (`REVIEW.md`) |
| `ferrus://submission` | Executor submission notes (`SUBMISSION.md`) |
| `ferrus://question` | Pending human question (`QUESTION.md`) |
| `ferrus://state` | Current task state as JSON (`STATE.json`) |

### MCP prompts

Bundled context prompts that stitch together the most relevant files for each role:

| Prompt | Description |
|---|---|
| `executor-context` | State + task + feedback + review notes |
| `supervisor-review` | State + task + submission notes |

---

## `ferrus.toml`

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
```

`STATE.json` is written atomically (write to a `.tmp` file, then rename) so a crash mid-write never leaves it corrupt.

The check commands run in the directory where `ferrus serve` was started. Any non-zero exit code is a failure. On failure, the full stdout + stderr for each command is written to `.ferrus/logs/check_<attempt>_<timestamp>.txt`. `FEEDBACK.md` contains a short summary — which commands failed and the last `max_feedback_lines` lines of their output — so the Executor gets the signal without noise.

---

## Runtime files (`.ferrus/`)

| File | Contents |
|---|---|
| `STATE.json` | Current state, retry/cycle counters, failure reason, schema version, last-write timestamp (RFC 3339) and PID |
| `TASK.md` | Task description written by Supervisor |
| `FEEDBACK.md` | Short check-failure summary (failed commands, last N lines each, log path) |
| `REVIEW.md` | Supervisor rejection notes |
| `SUBMISSION.md` | Executor's submission notes (summary, verification steps, known limitations) |
| `QUESTION.md` | Question written by `/ask_human` when elicitation is unavailable |
| `ANSWER.md` | Answer written by `/answer` |
| `logs/check_<attempt>_<ts>.txt` | Full stdout + stderr for each check run |

`.ferrus/` is gitignored by `ferrus init`.
