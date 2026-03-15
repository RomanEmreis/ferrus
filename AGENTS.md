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

## Working as an Executor via ferrus

If you are running as an Executor under ferrus orchestration, your skill file at `.agents/skills/ferrus-executor/SKILL.md` describes your full workflow. In brief:

1. Call `/wait_for_task` — returns `{"status":"claimed", "task":"...", ...}` or `{"status":"timeout", "state":"..."}`
2. Implement the changes described in `task`
3. Call `/heartbeat` approximately every 30 seconds to keep your lease alive
4. Call `/check` — fix failures and repeat until all checks pass
5. Call `/submit` with a summary, verification steps, and any known limitations

Read `.agents/skills/ferrus-executor/SKILL.md` for the authoritative workflow.
