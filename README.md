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
  │ create_task         │            │ next_task           │
  │ review_pending      │            │ check               │
  │ approve / reject    │            │ submit              │
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
                     │  logs/       │
                     └──────────────┘
```

The human switches context between agents; only one is active at a time in the MVP.

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

`/reset` moves `Failed → Idle` (human intervention).

---

## Installation

```sh
cargo install --path .
```

---

## Quick start

```sh
# In your project directory:
ferrus init

# Write MCP config files for your agents:
ferrus register --supervisor claude-code --executor codex

# Agents can now launch ferrus automatically via their MCP config.
# Or start manually:
ferrus serve --role supervisor   # Supervisor session
ferrus serve --role executor     # Executor session
ferrus serve                     # All tools (single-agent / debug)
```

---

## Commands

### `ferrus init`

Scaffolds ferrus in the current project:

- Creates `ferrus.toml` with default check commands and limits
- Creates `.ferrus/` with `STATE.json`, `TASK.md`, `FEEDBACK.md`, `REVIEW.md`, `SUBMISSION.md`, and `logs/`
- Adds `.ferrus/` to `.gitignore`

### `ferrus serve [--role supervisor|executor]`

Starts the MCP server on stdio. Pass `--role` to expose only the tools relevant to that agent:

| `--role` | Tools exposed |
|---|---|
| `supervisor` | `create_task`, `review_pending`, `approve`, `reject`, `status`, `reset` |
| `executor` | `next_task`, `check`, `submit`, `status`, `reset` |
| *(omitted)* | All 9 tools |

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
| `review_pending` | Reviewing | — | Read task + context for review |
| `approve` | Reviewing | Complete | Accept the submission |
| `reject` | Reviewing | Addressing | Reject with notes; resets Executor retry counter |

### Executor tools

| Tool | From state | To state | Description |
|---|---|---|---|
| `next_task` | Executing, Addressing | — | Read task + any feedback/review notes |
| `check` | Executing, Addressing | Checking / Addressing / Failed | Run all configured checks |
| `submit` | Checking | Reviewing | Write submission notes + signal ready for Supervisor review |

### Shared tools

| Tool | From state | To state | Description |
|---|---|---|---|
| `status` | any | — | Print current state + retry counters |
| `reset` | Failed | Idle | Human escape hatch; clears feedback, review, and submission files |

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
max_check_retries = 5   # consecutive check failures before state → Failed
max_review_cycles = 3   # reject→fix cycles before state → Failed
max_feedback_lines = 30 # trailing lines per failing command shown in FEEDBACK.md
```

The check commands run in the directory where `ferrus serve` was started. Any non-zero exit code is a failure. On failure, the full stdout + stderr for each command is written to `.ferrus/logs/check_<attempt>_<timestamp>.txt`. `FEEDBACK.md` contains a short summary — which commands failed and the last `max_feedback_lines` lines of their output — so the Executor gets the signal without noise.

---

## Runtime files (`.ferrus/`)

| File | Contents |
|---|---|
| `STATE.json` | Current state, retry/cycle counters, failure reason |
| `TASK.md` | Task description written by Supervisor |
| `FEEDBACK.md` | Short check-failure summary (failed commands, last N lines each, log path) |
| `REVIEW.md` | Supervisor rejection notes |
| `SUBMISSION.md` | Executor's submission notes (summary, verification steps, known limitations) |
| `logs/check_<attempt>_<ts>.txt` | Full stdout + stderr for each check run |

`.ferrus/` is gitignored by `ferrus init`.
