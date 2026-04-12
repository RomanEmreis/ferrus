# AGENTS.md

Coding guidance for AI agents working in this repository.

## Project

`ferrus` is a Rust AI agent orchestrator for software projects. It drives a **Supervisor–Executor** workflow: the Supervisor plans tasks and reviews submissions, the Executor implements and checks its own work. State is shared via `.ferrus/` on disk; coordination uses MCP as an implementation detail.

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

All three checks must pass before submitting: `clippy -D warnings`, `fmt --check`, `test`.

## Source Layout

```
src/
  main.rs                    # CLI entry, tracing init, HQ logger
  cli/                       # clap subcommands (init, serve, register)
  config/mod.rs              # Deserialize ferrus.toml (ChecksConfig, LimitsConfig, LeaseConfig, HqConfig)
  state/machine.rs           # TaskState enum + StateData + transition methods + lease helpers
  state/store.rs             # Async read/write of .ferrus/ files; open_lock_file, claim_state
  state/agents.rs            # AgentEntry, AgentsRegistry — .ferrus/agents.json lifecycle tracking
  checks/runner.rs           # Spawn check subprocesses, collect output
  hq/mod.rs                  # HQ entry point; HqContext; tokio::select! loop; transition_action
  hq/state_watcher.rs        # Background task: polls STATE.json every 250ms, watch channel
  hq/tui.rs                  # Terminal UI (crossterm): App event loop, UiMessage, StatusSnapshot; autocomplete, command history, status line, confirmation dialogs; double-Ctrl+C-to-quit (2-second window)
  hq/commands.rs             # ShellCommand enum, parse_command() via clap + shlex
  hq/display.rs              # Display wrapper: sends UiMessage to TUI channel (info, error, transition, status, suspend, resume, confirm)
  hq/agent_manager.rs        # agent spawn helpers (headless for executor and supervisor); HeadlessHandle; agents.json updates
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

**HARD RULE — no exceptions: NEVER run check commands manually** (`cargo test`, `cargo clippy`, `cargo build`, `npm test`, `make`, `pytest`, or any build/test/lint command). Always use the `/check` MCP tool — it records results, updates state, and handles retry counting. Running checks manually bypasses the state machine entirely: retry counters won't increment, FEEDBACK.md won't be updated, and state transitions won't fire.

Full workflow: `.agents/skills/ferrus-executor/SKILL.md`

## Ferrus Supervisor

This repository is orchestrated by Ferrus HQ.

Your initial prompt tells you which mode you are in. Match it exactly.

**Task-definition mode** ("You are a Ferrus Supervisor in TASK DEFINITION mode"): Interview the user, draft the exact task text, show that draft to the user, gather feedback, and call `/create_task` only after the user explicitly approves the task text. HQ terminates this session once `/create_task` succeeds.

MUST NOT in task-definition mode:
- MUST NOT write, edit, or create any files
- MUST NOT run commands or implement code
- MUST NOT explore the codebase to design a solution yourself
- MUST NOT call `/create_task` before the user has explicitly approved the task text

**Review mode** ("You are a Ferrus Supervisor in REVIEW mode"): Call `/wait_for_review`, then `/review_pending`, then `/approve` or `/reject`. After deciding, **exit**.

MUST NOT in review mode:
- MUST NOT implement fixes or changes yourself
- MUST NOT ask the Executor to re-verify

**Free-form plan mode** ("You are a Ferrus Supervisor in free-form planning mode"): No hard constraints. Explore, discuss, write plans. `/create_task` is available but not required.

**Consultation mode** ("You are a Ferrus Supervisor in CONSULTATION mode"): Read `TASK.md` + `CONSULT_REQUEST.md`, investigate read-only, call `/respond_consult`, then exit. You may use `/ask_human` if the answer cannot be determined from the repository.

See `.agents/skills/ferrus-supervisor/SKILL.md` for the full workflow.
