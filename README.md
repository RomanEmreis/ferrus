# ferrus

[![Ferrus version](https://img.shields.io/badge/ferrus-0.2.5--alpha.5-orange)](https://crates.io/crates/ferrus)
[![Rust version](https://img.shields.io/badge/rustc-1.93+-964B00)](https://releases.rs/docs/1.93.0/)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](https://github.com/RomanEmreis/ferrus/blob/main/LICENSE)
[![Rust](https://github.com/RomanEmreis/ferrus/actions/workflows/rust.yml/badge.svg)](https://github.com/RomanEmreis/ferrus/actions/workflows/rust.yml)
[![Publish](https://github.com/RomanEmreis/ferrus/actions/workflows/publish.yml/badge.svg)](https://github.com/RomanEmreis/ferrus/actions/workflows/publish.yml)

**Deterministic orchestration of AI agents for real software work.**

Ferrus turns coding agents into controlled, repeatable workers.

It runs a Supervisor → Executor → Reviewer loop over your repository — not as a chat, but as a **state machine**.  
Tasks are planned, implemented, checked, and reviewed in a structured, restart-safe flow. Unlike chat-based agents, ferrus enforces structure and lifecycle.

Everything is explicit:
- State lives on disk (`.ferrus/`)
- Agents are stateless between runs
- Crashes are recoverable
- No hidden context

## Supported agents

Ferrus works with existing coding agents:

- **Codex**
- **Claude Code**
- **Qwen Code** (experimental)

Agents are treated as interchangeable workers — ferrus provides the runtime, coordination, and state.

> 💡 **Status**: ferrus is currently in alpha and not ready for production.

[Tutorial](https://romanemreis.github.io/ferrus-docs/) | [Roadmap](https://github.com/RomanEmreis/ferrus/blob/main/docs/milestones.md)

---

## How it works

```
  you
   │
   └─► ferrus HQ
         │
         ├─► Supervisor (Claude Code or Codex) — plans tasks
         │         │ exits after task created;
         │
         ├─► Executor (Claude Code or Codex)   — implements, checks, submits
         │         │ runs headlessly
         │
         └─► Reviewer (Claude Code or Codex)   — spawned automatically on submission
                   │ exits after approve/reject; runs headlessly
```

HQ watches state transitions and spawns the right agent at the right time.

State is shared through `.ferrus/` on disk — plain text files agents read and write via their tools. If an agent crashes and restarts, it picks up exactly where it left off.

---

## Quick start

```sh
cargo install ferrus
# or:
curl -fsSL https://github.com/RomanEmreis/ferrus/releases/latest/download/install.sh | sh

ferrus init                                                # scaffold ferrus.toml + .ferrus/
ferrus register --supervisor claude-code --executor codex  # write agent configs
ferrus                                                     # enter HQ
```

Then type `/task` — a supervisor spawns, you describe what you want, and the full loop runs automatically.

On Linux `x86_64` and `aarch64`, the installer downloads the latest release binary into `~/.local/bin` by default. Release archives are verified with published SHA-256 checksums before installation. Set `FERRUS_INSTALL_DIR` to override the destination, or `FERRUS_INSTALL_VERSION=vX.Y.Z` to install a specific release tag.

---

## HQ

`ferrus` with no arguments opens an interactive shell:

| Command | Description |
|---|---|
| `/plan` | Free-form planning session with the supervisor (no task created) |
| `/task` | Define a task with the supervisor, then run the executor→review loop automatically |
| `/supervisor` | Open an interactive supervisor session (no initial prompt) |
| `/executor` | Open an interactive executor session (no initial prompt) |
| `/resume` | Manually resume the executor headlessly; also recovers Consultation by relaunching both supervisor and executor |
| `/review` | Manually spawn supervisor in review mode (escape hatch when automatic spawning failed) |
| `/status` | Show task state, agent list, and session log paths |
| `/attach <name>` | Show log path for a running headless agent |
| `/stop` | Stop all running agent sessions (prompts for confirmation) |
| `/reset` | Reset state to Idle and clear task files (prompts for confirmation) |
| `/init [--agents-path]` | Initialize ferrus in the current directory |
| `/register` | Register agent configs (same as `ferrus register`) |
| `/model` | Update the supervisor or executor model override |
| `/help` | List all HQ commands |
| `/quit` | Exit HQ |

> **Quit HQ:** Press **Ctrl+C** twice within 2 seconds to exit. The first press shows a yellow "Press Ctrl+C again to exit" prompt in the status line; the second confirms and exits. The prompt clears automatically after 2 seconds if you change your mind.

> **TUI features:** Type `/` to see autocomplete suggestions; press **Tab** / **Shift+Tab** to navigate and **Enter** to accept. A status line at the bottom of the terminal shows the current task state and retry/cycle counters in real time.

### How the loop works

```
ferrus> /task
  └─ supervisor spawns → you describe the task → supervisor calls create_task
       └─ executor spawns (headless) → implements → check → submit
            └─ reviewer spawns (headless) → reads submission → approve or reject
                 ├─ approved → Complete
                 └─ rejected → executor re-spawns with feedback
```

Agents are **stateless between runs** — context lives in `.ferrus/*.md`. Each spawn receives a short prompt pointing to those files and exits when its job is done.

---

## State machine

```
Idle
 └─► Executing                              ← create_task
       ├─► Consultation                     ← consult
       │     └─► Executing                  ← wait_for_consult
       ├─► Executing                        ← check / submit (final check failed, retries < max)
       ├─► Failed                           ← check / submit (final check failed, retries ≥ max)
       └─► Reviewing                        ← submit (final check passed)
             ├─► Complete                   ← approve
             ├─► Failed                     ← reject (cycles ≥ max)
             └─► Addressing                 ← reject (cycles < max)
                   ├─► Consultation         ← consult
                   │     └─► Addressing     ← wait_for_consult
                   ├─► Addressing           ← check / submit (final check failed, retries < max)
                   ├─► Failed               ← check / submit (final check failed, retries ≥ max)
                   └─► Reviewing            ← submit (final check passed)
```

Any active Executor work state (Executing, Addressing, Checking) can pause to `Consultation` via `/consult`. HQ spawns the configured Supervisor in consultation mode, and the executor immediately calls `/wait_for_consult` to block until the Supervisor answers via `/respond_consult`.

Any active state, including `Consultation`, can pause to `AwaitingHuman` via `/ask_human`. The agent immediately calls `/wait_for_answer` to block until the human responds. The human types their answer in the HQ terminal (raw text, no slash prefix). `/wait_for_answer` restores the previous state and returns the answer.

- `/task` from `Complete` → silently resets to Idle and starts the next task (no extra step needed).
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

### `ferrus serve [--role supervisor|executor] [--agent-name <name>] [--agent-index <n>]`

Starts the agent coordination server on stdio. Agents load this as an MCP server. `--agent-name` and `--agent-index` are embedded in the `claimed_by` field (e.g. `"executor:codex:1"`). Pass `--role` to expose only the tools for that role:

| `--role` | Tools exposed |
|---|---|
| `supervisor` | `create_task`, `wait_for_review`, `review_pending`, `approve`, `reject`, `respond_consult`, `ask_human`, `answer`, `status`, `reset`, `heartbeat` |
| `executor` | `wait_for_task`, `check`, `consult`, `submit`, `wait_for_consult`, `wait_for_answer`, `ask_human`, `answer`, `status`, `reset`, `heartbeat` |
| *(omitted)* | All tools |

### `ferrus register --supervisor <agent> [--supervisor-model <model>] --executor <agent> [--executor-model <model>]`

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
max_check_retries = 20   # consecutive check failures before state → Failed
max_review_cycles = 3    # reject→fix cycles before state → Failed
max_feedback_lines = 30  # trailing lines per failing command shown in /check and /submit output
wait_timeout_secs = 60   # max duration of one wait_* tool call before it returns timeout so the agent can poll again

[lease]
ttl_secs = 90                  # how long a claimed lease is valid without renewal
heartbeat_interval_secs = 30   # how often agents should call heartbeat

[hq.supervisor]
agent = "claude-code"  # agent for supervisor/reviewer role: claude-code | codex
model = ""             # optional override; empty = agent default

[hq.executor]
agent = "codex"        # agent for executor role: claude-code | codex
model = ""             # optional override; empty = agent default
```

Check commands run in the directory where `ferrus serve` was started. Full output is written to `.ferrus/logs/check_<attempt>_<ts>.txt`. `/check` and `/submit` return a short failure summary inline so the Executor gets the signal without persisting technical noise into task context.

---

## Runtime files (`.ferrus/`)

| File | Contents |
|---|---|
| `STATE.json` | Current state, lease fields, retry/cycle counters, schema version, timestamp |
| `STATE.lock` | Advisory lock file for atomic claiming (do not delete) |
| `TASK.md` | Task description written by Supervisor |
| `REVIEW.md` | Supervisor rejection notes |
| `SUBMISSION.md` | Executor submission notes |
| `QUESTION.md` | Pending human question (written by `/ask_human`) |
| `ANSWER.md` | Human answer |
| `CONSULT_REQUEST.md` | Pending supervisor consultation request |
| `CONSULT_RESPONSE.md` | Supervisor consultation response |
| `logs/` | Full stdout + stderr per check run; PTY session logs per agent |

`STATE.json` is written atomically (write to `.tmp`, then rename) so a crash mid-write never leaves it corrupt. `.ferrus/` is gitignored by `ferrus init`.

---

## Dogfooding

Ferrus is partially developed using its own orchestration workflow.

This repository is used to validate the Supervisor → Executor → Reviewer loop in real development scenarios.

---

## Getting involved

If you're interested in Ferrus:

- Try running it on your project
- Share feedback on the workflow (what breaks, what feels unnatural)
- Open issues with observations or ideas

At this stage, feedback on the model is more valuable than code contributions.

---

## Licence

Licensed under Apache 2.0.
