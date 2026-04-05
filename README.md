# ferrus

An AI agent orchestrator for software projects. You run `ferrus` in your project directory and it drives a **Supervisor–Executor** workflow: the Supervisor plans tasks and reviews submissions, the Executor implements and checks its own work. You watch from HQ, attach to any agent's terminal, and let the loop run.

Licensed under Apache 2.0.

---

## How it works

```
  you
   │
   └─► ferrus HQ
         │
         ├─► Supervisor (Claude Code)   — plans tasks, reviews submissions
         │         │ exits after task created
         │
         ├─► Executor (Codex / any)     — implements, checks, submits
         │         │ runs in background PTY
         │
         └─► Reviewer (Claude Code)     — spawned automatically on submission
                   │ exits after approve/reject
```

Each agent runs headlessly in a background PTY session. HQ watches state transitions and spawns the right agent at the right time. You can attach to any session at any point with `/attach <name>` to observe or interact.

State is shared through `.ferrus/` on disk — plain text files agents read and write via their tools. If an agent crashes and restarts, it picks up exactly where it left off.

---

## Quick start

```sh
cargo install --path .

ferrus init                                              # scaffold ferrus.toml + .ferrus/
ferrus register --supervisor claude-code --executor codex  # write agent configs
ferrus                                                   # enter HQ
```

Then type `/plan` — a supervisor spawns, you describe what you want, and the full loop runs automatically.

---

## HQ

`ferrus` with no arguments opens an interactive shell:

| Command | Description |
|---|---|
| `/plan` | Spawn supervisor to plan a task, then drive executor→review loop automatically |
| `/execute` | Manually start or resume the executor (escape hatch if automatic spawning failed) |
| `/review` | Manually spawn supervisor in review mode (if automatic spawning failed or HQ restarted) |
| `/status` | Show task state, agent list, and PTY session log paths |
| `/attach <name>` | Attach terminal to a running background session (e.g. `executor-1`); auto-detaches when the agent's task completes |
| `/stop` | Stop all running agent sessions (prompts for confirmation) |
| `/reset` | Reset state to Idle and clear task files (prompts for confirmation) |
| `/init [--agents-path]` | Initialize ferrus in the current directory |
| `/register` | Register agent configs (same as `ferrus register`) |
| `/help` | List all HQ commands |
| `/quit` | Exit HQ |

> **Detach key:** While attached to a session, press **Ctrl+]** then **d** to detach without killing the agent.
> - Ctrl+] is the prefix — pressing it alone does nothing visible (it's held until the next key).
> - Ctrl+] d → detach and return to HQ.
> - Ctrl+] Ctrl+] → send a literal Ctrl+] to the agent (escape hatch).
> - Ctrl+] is ASCII 0x1D (GS) — not intercepted by tmux, readline, or Claude Code.

> **TUI features:** Type `/` to see autocomplete suggestions; press **Tab** / **Shift+Tab** to navigate and **Enter** to accept. A status line at the bottom of the terminal shows the current task state and retry/cycle counters in real time.

### How the loop works

```
ferrus> /plan
  └─ supervisor spawns (foreground) → you describe the task → supervisor calls create_task
       └─ executor spawns (background PTY) → implements → check → submit
            └─ reviewer spawns (background PTY) → reads submission → approve or reject
                 ├─ approved → Complete
                 └─ rejected → executor re-spawns with feedback
```

Agents are **stateless between runs** — context lives in `.ferrus/*.md`. Each spawn receives a short prompt pointing to those files and exits when its job is done.

---

## State machine

```
Idle
 └─► Executing      ← create_task (Supervisor)
       └─► Checking ← check (Executor, pass)
             ├─► [FAIL, retries < max] Addressing → check again
             ├─► [FAIL, retries ≥ max] Failed
             └─► Reviewing ← submit (Executor)
                   ├─► [REJECT] Addressing → check loop (retries reset)
                   │     └─► [cycles ≥ max] Failed
                   └─► Complete ← approve (Supervisor)
```

Any active state can pause to `AwaitingHuman` when an agent needs human input.

- `/plan` from `Complete` → silently resets to Idle and starts the next task (no extra step needed).
- `/reset` → Idle from any state; prompts for confirmation if an agent is actively working.

---

## CLI reference

### `ferrus init [--agents-path <path>]`

Scaffolds ferrus in the current project (default `--agents-path .agents`):

- Creates `ferrus.toml` with default check commands and limits
- Creates `.ferrus/` runtime directory with all state files and `logs/`
- Creates skill files agents load to understand their role:
  - `<agents-path>/skills/ferrus/SKILL.md` — general overview
  - `<agents-path>/skills/ferrus-supervisor/SKILL.md` + `ROLE.md`
  - `<agents-path>/skills/ferrus-executor/SKILL.md` + `ROLE.md`
- Adds `.ferrus/` to `.gitignore`

### `ferrus serve [--role supervisor|executor]`

Starts the agent coordination server on stdio. Agents load this as an MCP server. Pass `--role` to expose only the tools for that role:

| `--role` | Tools exposed |
|---|---|
| `supervisor` | `create_task`, `wait_for_review`, `review_pending`, `approve`, `reject`, `ask_human`, `answer`, `status`, `reset` |
| `executor` | `wait_for_task`, `next_task`, `check`, `submit`, `ask_human`, `answer`, `status`, `reset` |
| *(omitted)* | All tools |

### `ferrus register --supervisor <agent> --executor <agent>`

Writes agent config files so they automatically load `ferrus serve` as a tool server. Supported agents:

| Agent | Config written |
|---|---|
| `claude-code` | `.mcp.json` |
| `codex` | `.codex/config.toml` |

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
wait_timeout_secs = 3600 # poll timeout for wait_for_task / wait_for_review

[lease]
ttl_secs = 90                  # how long a claimed lease is valid without renewal
heartbeat_interval_secs = 30   # how often agents should call heartbeat

[hq]
supervisor = "claude-code"  # agent for supervisor/reviewer role: claude-code | codex
executor = "codex"          # agent for executor role: claude-code | codex
```

Check commands run in the directory where `ferrus serve` was started. Full output is written to `.ferrus/logs/check_<attempt>_<ts>.txt`. `FEEDBACK.md` contains a short summary so the Executor gets the signal without noise.

---

## Runtime files (`.ferrus/`)

| File | Contents |
|---|---|
| `STATE.json` | Current state, lease fields, retry/cycle counters, schema version, timestamp |
| `STATE.lock` | Advisory lock file for atomic claiming (do not delete) |
| `TASK.md` | Task description written by Supervisor |
| `FEEDBACK.md` | Short check-failure summary (failed commands + last N lines each + log path) |
| `REVIEW.md` | Supervisor rejection notes |
| `SUBMISSION.md` | Executor submission notes |
| `QUESTION.md` | Pending human question (when agent used ask_human without elicitation) |
| `ANSWER.md` | Human answer |
| `logs/` | Full stdout + stderr per check run; PTY session logs per agent |

`STATE.json` is written atomically (write to `.tmp`, then rename) so a crash mid-write never leaves it corrupt. `.ferrus/` is gitignored by `ferrus init`.
