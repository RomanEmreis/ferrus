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

Any active state can pause to `AwaitingHuman` via `/ask_human`. `/answer` restores it.
`/reset` moves `Failed → Idle`.

## CLI

```sh
ferrus init [--agents-path <path>]              # scaffold project files and skill files
ferrus serve [--role supervisor|executor]       # start MCP server on stdio
ferrus register --supervisor <a> --executor <a> # write MCP config for agents
```

Set `RUST_LOG=ferrus=debug` (or `info`/`warn`) for verbose logs to stderr.

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

### Shared
| Tool | From state | Description |
|---|---|---|
| `ask_human` | any active | Ask human a question (elicitation or AwaitingHuman fallback) |
| `answer` | AwaitingHuman | Provide answer; restores previous state |
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
