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
  main.rs                    # CLI entry, tracing init, HQ logger
  cli/                       # clap subcommands (init, serve, register)
  config/mod.rs              # Deserialize ferrus.toml (ChecksConfig, LimitsConfig, LeaseConfig, HqConfig)
  state/machine.rs           # TaskState enum + StateData + transition methods + lease helpers
  state/store.rs             # Async read/write of .ferrus/ files; open_lock_file, claim_state
  state/agents.rs            # AgentEntry, AgentsRegistry — .ferrus/agents.json lifecycle tracking
  pty.rs                     # BackgroundSession, spawn_background, Ctrl-B d FSM, attach()
  checks/runner.rs           # Spawn check subprocesses, collect output
  hq/mod.rs                  # HQ entry point; HqContext; tokio::select! loop; transition_action
  hq/state_watcher.rs        # Background task: polls STATE.json every 250ms, watch channel
  hq/repl.rs                 # readline_loop (rustyline, runs in spawn_blocking)
  hq/commands.rs             # ShellCommand enum, parse_command() via clap + shlex
  hq/display.rs              # print_status, print_transition, print_info, print_error
  hq/agent_manager.rs        # spawn_and_wait, spawn_background_pty, kill_role; agents.json updates
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

This repository is orchestrated by Ferrus HQ.

When spawned by `ferrus` HQ, your initial prompt will tell you what to do.

If started manually: call MCP tool `/wait_for_task` as your first action.

Full workflow: `.agents/skills/ferrus-executor/SKILL.md`

## Ferrus Supervisor

This repository is orchestrated by Ferrus HQ.

The Supervisor runs in one of two modes — check your initial prompt:

**Plan mode** ("You are in planning mode"): Collaborate with the user to define the task, then call `/create_task`. After `/create_task` succeeds, **exit immediately**. Do NOT call `/wait_for_review`.

**Review mode** ("You are in review mode"): Call `/wait_for_review`, then `/review_pending` to read TASK.md + SUBMISSION.md, then `/approve` or `/reject`. After deciding, **exit**.

See `.agents/skills/ferrus-supervisor/SKILL.md` for the full two-mode workflow.
