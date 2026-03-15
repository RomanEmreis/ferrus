# AGENTS.md

Coding guidance for AI agents working in this repository.

## Project

`ferrus` is a Rust MCP server that coordinates AI agents: a **Supervisor** (plans, reviews) and one or more **Executors** (writes code, fixes fixes). State is shared via `.ferrus/` on disk.

Licensed under Apache 2.0.

## Build & Test

```sh
cargo build                        # compile
cargo build --release              # optimized build
cargo test                         # run all tests
cargo test <name>                  # run a single test by name
cargo clippy -- -D warnings        # lint (warnings are errors)
cargo fmt                          # format
cargo fmt --check                  # check formatting without writing
cargo check                        # fast type-check
```

All four checks must pass before submitting: `clippy -D warnings`, `fmt --check`, `test`.

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
  server/tools/              # One file per MCP tool (one module = one tool)
  server/resources.rs        # MCP resource handler (ferrus://{file})
  server/prompts.rs          # MCP prompt handlers
```

## Key Patterns

**Tool files** expose `pub const DESCRIPTION: &str`, optionally `pub const INPUT_SCHEMA: &str`, and `pub async fn handler(...)`. Registered manually via `app.map_tool()` in `server/mod.rs` — no macros.

**State reads/writes**: always read `STATE.json` at tool entry via `store::read_state()`, write at exit via `store::write_state()`. Never reconstruct `StateData::default()` mid-tool — use `..state.clone()` spread to preserve lease fields.

**Lease fields**: `claimed_by`, `lease_until`, `last_heartbeat` on `StateData`. Cleared by transition methods (`create_task`, `submit`, `approve`, `reject`) — not by tool files. Never clear them manually in a tool.

**File locking**: `wait_for_task`, `wait_for_review`, and `/heartbeat` acquire an exclusive `flock` on `.ferrus/STATE.lock` (not `STATE.json`) for their read-check-write cycle. Use `store::open_lock_file()` + `tokio::task::spawn_blocking` for the blocking lock call.

## Ferrus Executor

This repository is orchestrated by Ferrus.

Executor agents must not begin work until a task is claimed.

**First action:** call MCP tool `/wait_for_task`.

Do not explore the repository before claiming a task.

Full workflow: `.agents/skills/ferrus-executor/SKILL.md`

## Ferrus Supervisor

Supervisor agents must not create tasks without first checking the current state.

**First action:** call MCP tool `/status`.

Then follow `.agents/skills/ferrus-supervisor/SKILL.md` — create a task if state is `Idle`, or pick up the review flow if a task is already in progress.
