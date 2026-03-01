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
ferrus init    # scaffold ferrus.toml and .ferrus/ in the current project
ferrus serve   # start the MCP server on stdio
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
max_check_retries = 5   # consecutive check failures before state → Failed
max_review_cycles = 3   # reject→fix cycles before state → Failed
```

## MCP Tool Reference

### Supervisor tools

| Tool | From state | To state | Description |
|---|---|---|---|
| `/create_task` | Idle | Executing | Write task description; Executor picks it up |
| `/review_pending` | Reviewing | — | Read task + context for review |
| `/approve` | Reviewing | Complete | Accept the submission |
| `/reject` | Reviewing | Addressing | Reject with notes; resets Executor retry counter |

### Executor tools

| Tool | From state | To state | Description |
|---|---|---|---|
| `/next_task` | Executing, Addressing | — | Read task + any feedback/review notes |
| `/check` | Executing, Addressing | Checking / Addressing / Failed | Run all configured checks |
| `/submit` | Checking | Reviewing | Signal checks passed; ready for Supervisor review |

### Shared tools

| Tool | From state | To state | Description |
|---|---|---|---|
| `/status` | any | — | Print current state + retry counters |
| `/reset` | Failed | Idle | Human escape hatch; clears feedback + review files |

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

`/reset`: Failed → Idle (human intervention).

## Runtime Files (`.ferrus/`)

| File | Contents |
|---|---|
| `STATE.json` | Current `TaskState`, retry/cycle counters, failure reason |
| `TASK.md` | Task description written by Supervisor |
| `FEEDBACK.md` | Aggregated check failure output |
| `REVIEW.md` | Supervisor rejection notes |

`.ferrus/` is gitignored by `ferrus init`.

## Source Layout

```
src/
  main.rs                    # CLI entry, tracing init
  cli/                       # clap subcommands (init, serve)
  config/mod.rs              # Deserialize ferrus.toml
  state/machine.rs           # TaskState enum + transition methods
  state/store.rs             # Async read/write of .ferrus/ files
  checks/runner.rs           # Spawn check subprocesses, collect output
  server/mod.rs              # neva App setup (stdio transport)
  server/tools/              # One file per MCP tool
```
