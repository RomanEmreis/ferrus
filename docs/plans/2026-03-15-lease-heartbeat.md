# Lease & Heartbeat Foundation Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `claimed_by`/`lease_until`/`last_heartbeat` to `STATE.json`, atomic file-lock claiming to `wait_for_*`, a `/heartbeat` renewal tool, `[lease]` config, `--agent-name`/`--agent-index` CLI flags, and index auto-assignment in `ferrus register`.

**Architecture:** All state lives in `.ferrus/STATE.json` (no in-process shared state). A dedicated `.ferrus/STATE.lock` file (stable inode) is used for advisory `flock` mutual exclusion across processes, covering `wait_for_task`, `wait_for_review`, and `/heartbeat`. Lease fields are cleared by state transition methods in `machine.rs`, not by tool files. `agent_id` is constructed at server startup from CLI flags and captured into tool handler closures via `Arc<String>`.

**Tech Stack:** Rust, Tokio, `fs2 = "0.4"` (advisory file locking), `chrono` (already present), `neva` MCP framework, `serde_json`, `toml`, `clap`.

---

**Spec:** `docs/superpowers/specs/2026-03-15-lease-heartbeat-design.md`

---

## File Map

| File | Action | Summary |
|---|---|---|
| `Cargo.toml` | Modify | Add `fs2 = "0.4"` |
| `src/config/mod.rs` | Modify | Add `LeaseConfig`, `#[serde(default)]` on `Config` |
| `src/state/machine.rs` | Modify | 3 lease fields, 4 helpers (`is_claimed`, `is_claimed_by`, `lease_expired`, `clear_lease`), call `clear_lease` in 4 transitions |
| `src/state/store.rs` | Modify | Add `open_lock_file()`, `claim_state()` |
| `src/cli/mod.rs` | Modify | Add `--agent-name`/`--agent-index` to `Serve`; make `Register` flags `Option` |
| `src/cli/commands/serve.rs` | Modify | Pass `agent_name`/`agent_index` to `server::start()` |
| `src/cli/commands/register.rs` | Modify | `Agent::name()` helper, optional flags, index counting, `ferrus-{role}-{index}` key, `--agent-name` in args |
| `src/cli/commands/init.rs` | Modify | Create `STATE.lock` in `create_ferrus_dir()`; add `[lease]` to template; update 3 skill constants |
| `src/server/mod.rs` | Modify | Accept `agent_name`/`agent_index`, construct `agent_id`, closure-capture for 3 tools, register `/heartbeat` in shared block |
| `src/server/tools/wait_for_task.rs` | Modify | `handler(agent_id: &str)`, `STATE.lock` claim loop, structured JSON |
| `src/server/tools/wait_for_review.rs` | Modify | Same as `wait_for_task` but checks `Reviewing` state |
| `src/server/tools/heartbeat.rs` | Create | New tool: `STATE.lock` acquire, identity→expiry→state validation, renewal |
| `src/server/tools/mod.rs` | Modify | Add `pub mod heartbeat` |

---

## Chunk 1: Foundation — Deps, Config, State Machine

### Task 1: Add `fs2` dependency

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Add `fs2` to `Cargo.toml`**

In the `[dependencies]` section, after the `chrono` line, add:
```toml
fs2 = "0.4"
```

- [ ] **Step 2: Verify it compiles**

```sh
cargo build
```
Expected: compiles without errors.

- [ ] **Step 3: Commit**

```sh
git add Cargo.toml Cargo.lock
git commit -m "chore: add fs2 dependency for advisory file locking"
```

---

### Task 2: Add `LeaseConfig` to `config/mod.rs`

**Files:**
- Modify: `src/config/mod.rs`

- [ ] **Step 1: Write the failing test**

Add to the bottom of `src/config/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_config_defaults_without_block() {
        let toml = r#"
[checks]
commands = ["cargo test"]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.lease.ttl_secs, 90);
        assert_eq!(config.lease.heartbeat_interval_secs, 30);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

```sh
cargo test lease_config_defaults
```
Expected: compile error — `Config` has no field `lease`.

- [ ] **Step 3: Implement `LeaseConfig`**

Replace the current `Config` struct and its contents with:

```rust
use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub checks: ChecksConfig,
    pub limits: LimitsConfig,
    #[serde(default)]
    pub lease: LeaseConfig,
}

#[derive(Debug, Deserialize)]
pub struct ChecksConfig {
    pub commands: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct LimitsConfig {
    #[serde(default = "default_max_check_retries")]
    pub max_check_retries: u32,
    #[serde(default = "default_max_review_cycles")]
    pub max_review_cycles: u32,
    /// Maximum number of trailing lines shown per failing command in FEEDBACK.md.
    #[serde(default = "default_max_feedback_lines")]
    pub max_feedback_lines: usize,
    /// How long (in seconds) /wait_for_task and /wait_for_review poll before timing out.
    #[serde(default = "default_wait_timeout_secs")]
    pub wait_timeout_secs: u64,
}

#[derive(Debug, Deserialize)]
pub struct LeaseConfig {
    /// How long (in seconds) a claimed lease is valid without renewal.
    #[serde(default = "default_ttl_secs")]
    pub ttl_secs: u64,
    /// How often (in seconds) agents should call /heartbeat. Informational — not enforced server-side.
    #[serde(default = "default_heartbeat_interval_secs")]
    pub heartbeat_interval_secs: u64,
}

impl Default for LeaseConfig {
    fn default() -> Self {
        Self {
            ttl_secs: default_ttl_secs(),
            heartbeat_interval_secs: default_heartbeat_interval_secs(),
        }
    }
}

const fn default_max_check_retries() -> u32 { 5 }
const fn default_max_review_cycles() -> u32 { 3 }
const fn default_max_feedback_lines() -> usize { 30 }
const fn default_wait_timeout_secs() -> u64 { 3600 }
const fn default_ttl_secs() -> u64 { 90 }
const fn default_heartbeat_interval_secs() -> u64 { 30 }

impl Config {
    pub async fn load() -> Result<Self> {
        let contents = tokio::fs::read_to_string("ferrus.toml")
            .await
            .context("ferrus.toml not found — run `ferrus init` first")?;
        toml::from_str(&contents).context("Failed to parse ferrus.toml")
    }
}
```

- [ ] **Step 4: Run tests**

```sh
cargo test lease_config_defaults
```
Expected: PASS.

- [ ] **Step 5: Run full test suite + clippy**

```sh
cargo test && cargo clippy -- -D warnings
```
Expected: all pass.

- [ ] **Step 6: Commit**

```sh
git add src/config/mod.rs
git commit -m "feat: add LeaseConfig with ttl_secs and heartbeat_interval_secs defaults"
```

---

### Task 3: Add lease fields and helpers to `StateData`

**Files:**
- Modify: `src/state/machine.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)]` module in `src/state/machine.rs`:

```rust
    #[test]
    fn lease_helpers_unclaimed() {
        let s = StateData::default();
        assert!(!s.is_claimed());
        assert!(!s.is_claimed_by("executor:codex:1"));
        assert!(s.lease_expired()); // None counts as expired
    }

    #[test]
    fn lease_helpers_claimed() {
        use chrono::Duration;
        let mut s = StateData::default();
        s.claimed_by = Some("executor:codex:1".to_string());
        s.lease_until = Some(Utc::now() + Duration::seconds(60));
        s.last_heartbeat = Some(Utc::now());

        assert!(s.is_claimed());
        assert!(s.is_claimed_by("executor:codex:1"));
        assert!(!s.is_claimed_by("executor:codex:2"));
        assert!(!s.lease_expired());
    }

    #[test]
    fn lease_helpers_expired() {
        use chrono::Duration;
        let mut s = StateData::default();
        s.claimed_by = Some("executor:codex:1".to_string());
        s.lease_until = Some(Utc::now() - Duration::seconds(1)); // in the past
        s.last_heartbeat = Some(Utc::now() - Duration::seconds(31));

        assert!(!s.is_claimed());
        assert!(!s.is_claimed_by("executor:codex:1")); // expired = not claimed
        assert!(s.lease_expired());
    }
```

- [ ] **Step 2: Run to verify tests fail**

```sh
cargo test lease_helpers
```
Expected: compile error — fields and methods don't exist yet.

- [ ] **Step 3: Add fields to `StateData`**

In `src/state/machine.rs`, add three fields to `StateData` after `paused_state`:

```rust
    /// Agent that currently holds the task lease, e.g. "executor:codex:1".
    /// None when the task is unclaimed or in a terminal state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_by: Option<String>,
    /// Timestamp after which the lease is considered expired.
    /// None when unclaimed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_until: Option<DateTime<Utc>>,
    /// Timestamp of the last /heartbeat call.
    /// None when unclaimed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_heartbeat: Option<DateTime<Utc>>,
```

- [ ] **Step 4: Add helpers to `impl StateData`**

Add after the existing helpers (before `create_task`):

```rust
    /// True if a non-expired lease exists (`lease_until` is set and in the future).
    pub fn is_claimed(&self) -> bool {
        self.lease_until.map_or(false, |t| Utc::now() < t)
    }

    /// True if this specific agent holds a valid (non-expired) lease.
    pub fn is_claimed_by(&self, agent_id: &str) -> bool {
        self.claimed_by.as_deref() == Some(agent_id) && self.is_claimed()
    }

    /// True when `lease_until` is `None` or has been reached/passed.
    /// Returns `true` for unclaimed state so `!state.is_claimed()` is the correct
    /// claim check in `wait_for_task`.
    pub fn lease_expired(&self) -> bool {
        self.lease_until.map_or(true, |t| Utc::now() >= t)
    }

    /// Clear all lease fields. Called by transition methods that hand off ownership
    /// between roles or to a terminal state.
    pub fn clear_lease(&mut self) {
        self.claimed_by = None;
        self.lease_until = None;
        self.last_heartbeat = None;
    }
```

- [ ] **Step 5: Run tests**

```sh
cargo test lease_helpers
```
Expected: all PASS.

- [ ] **Step 6: Run full suite**

```sh
cargo test && cargo clippy -- -D warnings
```

- [ ] **Step 7: Commit**

```sh
git add src/state/machine.rs
git commit -m "feat: add lease fields and helpers to StateData"
```

---

### Task 4: Call `clear_lease()` in transition methods

**Files:**
- Modify: `src/state/machine.rs`

- [ ] **Step 1: Write failing tests**

Add to the test module:

```rust
    #[test]
    fn create_task_clears_lease() {
        use chrono::Duration;
        let mut s = idle();
        s.claimed_by = Some("supervisor:claude-code:1".to_string());
        s.lease_until = Some(Utc::now() + Duration::seconds(60));
        s.last_heartbeat = Some(Utc::now());
        s.create_task().unwrap();
        assert!(s.claimed_by.is_none());
        assert!(s.lease_until.is_none());
        assert!(s.last_heartbeat.is_none());
    }

    #[test]
    fn submit_clears_lease() {
        use chrono::Duration;
        let mut s = idle();
        s.create_task().unwrap();
        s.check_passed().unwrap();
        s.claimed_by = Some("executor:codex:1".to_string());
        s.lease_until = Some(Utc::now() + Duration::seconds(60));
        s.last_heartbeat = Some(Utc::now());
        s.submit().unwrap();
        assert!(s.claimed_by.is_none());
        assert!(s.lease_until.is_none());
        assert!(s.last_heartbeat.is_none());
    }

    #[test]
    fn approve_clears_lease() {
        use chrono::Duration;
        let mut s = idle();
        s.create_task().unwrap();
        s.check_passed().unwrap();
        s.submit().unwrap();
        s.claimed_by = Some("supervisor:claude-code:1".to_string());
        s.lease_until = Some(Utc::now() + Duration::seconds(60));
        s.last_heartbeat = Some(Utc::now());
        s.approve().unwrap();
        assert!(s.claimed_by.is_none());
        assert!(s.lease_until.is_none());
        assert!(s.last_heartbeat.is_none());
    }

    #[test]
    fn reject_clears_lease() {
        use chrono::Duration;
        let mut s = idle();
        s.create_task().unwrap();
        s.check_passed().unwrap();
        s.submit().unwrap();
        s.claimed_by = Some("supervisor:claude-code:1".to_string());
        s.lease_until = Some(Utc::now() + Duration::seconds(60));
        s.last_heartbeat = Some(Utc::now());
        s.reject(3).unwrap();
        assert!(s.claimed_by.is_none());
        assert!(s.lease_until.is_none());
        assert!(s.last_heartbeat.is_none());
    }

    #[test]
    fn reject_at_limit_clears_lease() {
        use chrono::Duration;
        let mut s = idle();
        s.create_task().unwrap();
        // Drive to the limit via two preceding reject cycles.
        for _ in 0..2 {
            s.check_passed().unwrap();
            s.submit().unwrap();
            s.reject(3).unwrap();
        }
        s.check_passed().unwrap();
        s.submit().unwrap();
        s.claimed_by = Some("supervisor:claude-code:1".to_string());
        s.lease_until = Some(Utc::now() + Duration::seconds(60));
        s.last_heartbeat = Some(Utc::now());
        // This call hits the limit-exceeded (Err) branch.
        let _ = s.reject(3);
        assert!(s.claimed_by.is_none());
        assert!(s.lease_until.is_none());
        assert!(s.last_heartbeat.is_none());
    }
```

- [ ] **Step 2: Run to see tests fail**

```sh
cargo test clears_lease
```
Expected: all FAIL (lease fields not cleared).

- [ ] **Step 3: Add `clear_lease()` calls to transition methods**

In `create_task()`, before setting `self.state = TaskState::Executing`:
```rust
        self.clear_lease();
        self.state = TaskState::Executing;
```

In `submit()`, before setting `self.state = TaskState::Reviewing`:
```rust
        self.clear_lease();
        self.state = TaskState::Reviewing;
```

In `approve()`, before setting `self.state = TaskState::Complete`:
```rust
        self.clear_lease();
        self.state = TaskState::Complete;
```

In `reject()`, inside the `Ok` branch before setting `self.state = TaskState::Addressing`:
```rust
        self.clear_lease();
        self.state = TaskState::Addressing;
```
And inside the `Err` branch (limit exceeded) before `self.state = TaskState::Failed`:
```rust
        self.clear_lease();
        self.state = TaskState::Failed;
```
Note: `reset()` already calls `*self = Self::default()` which zeroes all `Option` fields — no change needed.

- [ ] **Step 4: Run tests**

```sh
cargo test clears_lease
```
Expected: all PASS.

- [ ] **Step 5: Full suite**

```sh
cargo test && cargo clippy -- -D warnings
```

- [ ] **Step 6: Commit**

```sh
git add src/state/machine.rs
git commit -m "feat: clear lease fields on state transitions (submit, approve, reject, create_task)"
```

---

## Chunk 2: Infrastructure — Store, Init, CLI, Server

### Task 5: Add `open_lock_file` and `claim_state` to `store.rs`

**Files:**
- Modify: `src/state/store.rs`

- [ ] **Step 1: Add imports and helpers**

Add to the top of `src/state/store.rs`:
```rust
use chrono::Utc;
use std::fs::File;
```
(Note: `chrono::Utc` may already be in scope via `super::machine::StateData` — check and add only what's missing.)

Add these two public functions after `write_state`:

```rust
/// Open `.ferrus/STATE.lock` for use with `fs2::FileExt::lock_exclusive`.
/// The file must exist (created by `ferrus init`). Returns an open `std::fs::File`.
pub fn open_lock_file() -> Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .open(path("STATE.lock"))
        .with_context(|| "Cannot open .ferrus/STATE.lock — run `ferrus init` first")
}

/// Set the three lease fields on `state` and persist to disk.
/// Callers are responsible for holding the STATE.lock exclusive lock before
/// calling this function. Does not acquire the lock itself.
pub async fn claim_state(
    agent_id: &str,
    ttl_secs: u64,
    state: &mut StateData,
) -> Result<()> {
    let now = Utc::now();
    state.claimed_by = Some(agent_id.to_string());
    // chrono::Duration::try_seconds returns None only for values exceeding ~292 billion years;
    // the unwrap_or fallback to Duration::MAX is unreachable under any realistic TTL config.
    state.lease_until = Some(now + chrono::Duration::try_seconds(ttl_secs as i64)
        .unwrap_or(chrono::Duration::MAX));
    state.last_heartbeat = Some(now);
    write_state(state).await
}
```

- [ ] **Step 2: Verify compilation**

```sh
cargo build
```
Expected: compiles cleanly.

- [ ] **Step 3: Full suite**

```sh
cargo test && cargo clippy -- -D warnings
```

- [ ] **Step 4: Commit**

```sh
git add src/state/store.rs
git commit -m "feat: add open_lock_file and claim_state helpers to store"
```

---

### Task 6: Update `init.rs` — `STATE.lock`, `[lease]` template, skill constants

**Files:**
- Modify: `src/cli/commands/init.rs`

- [ ] **Step 1: Create `STATE.lock` in `create_ferrus_dir()`**

In the `create_ferrus_dir()` function, after the loop that creates the `.md` files, add:

```rust
    // Create the advisory lock file used by wait_for_task, wait_for_review, and /heartbeat
    let lock_path = dir.join("STATE.lock");
    if !lock_path.exists() {
        tokio::fs::write(&lock_path, "")
            .await
            .context("Failed to create .ferrus/STATE.lock")?;
        println!("Created .ferrus/STATE.lock");
    }
```

- [ ] **Step 2: Add `[lease]` block to `DEFAULT_FERRUS_TOML`**

Append to the `DEFAULT_FERRUS_TOML` constant (before the closing `"`):

```toml

[lease]
ttl_secs = 90              # how long a claimed lease is valid without renewal
heartbeat_interval_secs = 30 # how often agents should call /heartbeat
```

- [ ] **Step 3: Update `EXECUTOR_SKILL` constant**

Replace the `## Autonomous loop` section in `EXECUTOR_SKILL` with:

```rust
const EXECUTOR_SKILL: &str = r#"# Ferrus Executor

You are operating as an **Executor** in a ferrus-orchestrated project.
See [ROLE.md](./ROLE.md) for your full role definition and responsibilities.

## Autonomous loop

1. Call `/wait_for_task` — blocks until a task is assigned; returns JSON with `"status": "claimed"` or `"status": "timeout"`
   - On `"timeout"`: if you still hold a lease, call `/heartbeat` to renew it, then call `/wait_for_task` again
   - On `"claimed"`: read `task`, `feedback`, and `review` from the returned JSON
2. Implement the required changes
3. While working, call `/heartbeat` approximately every 30 seconds to keep your lease alive
4. Call `/check` — fix any failures and repeat until all checks pass
5. Call `/submit` with a summary, manual verification steps, and any known limitations
6. Return to step 1

## Notes

- Check failure details are in `.ferrus/FEEDBACK.md`; full logs are in `.ferrus/logs/`
- Call `/status` at any time to inspect current state and counters
- Call `/ask_human` if you need clarification from a human
- Use the `executor-context` MCP prompt for bundled task context
- Read runtime files as MCP resources: `ferrus://task`, `ferrus://feedback`, `ferrus://review`
"#;
```

- [ ] **Step 4: Update `SUPERVISOR_SKILL` constant**

Replace the `## Starting a new task` section in `SUPERVISOR_SKILL` with:

```rust
const SUPERVISOR_SKILL: &str = r#"# Ferrus Supervisor

You are operating as a **Supervisor** in a ferrus-orchestrated project.
See [ROLE.md](./ROLE.md) for your full role definition and responsibilities.

## Starting a new task

1. Call `/create_task` with a detailed Markdown description of what must be done
2. Call `/wait_for_review` — returns JSON with `"status": "claimed"` or `"status": "timeout"`
   - On `"timeout"`: call `/heartbeat` to renew your lease (if reviewing), then call `/wait_for_review` again
   - On `"claimed"`: read `task`, `submission`, `feedback`, and `review` from the returned JSON
3. While reviewing, call `/heartbeat` approximately every 30 seconds to keep your lease alive
4. Call `/approve` to accept, or `/reject` with clear and actionable notes
5. Return to step 2 for the next review cycle, or step 1 for a new task

## Resuming after a restart

Call `/wait_for_review` — it returns immediately if a submission is already pending,
otherwise blocks until the Executor submits.

## Notes

- Call `/status` at any time to inspect current state and counters
- Call `/ask_human` if you need clarification from a human
- Use the `supervisor-review` MCP prompt for bundled review context
- Read runtime files as MCP resources: `ferrus://task`, `ferrus://submission`, `ferrus://state`
"#;
```

- [ ] **Step 5: Update `FERRUS_SKILL` constant**

In `FERRUS_SKILL`, update the `### Shared` tools table to add `/heartbeat`:

```
| `heartbeat` | any claimed | Renew lease; returns `{"status":"renewed"}` or `{"status":"error","code":"..."}` |
```

Update the `## ferrus.toml` section to add:
```toml
[lease]
ttl_secs = 90            # lease validity without renewal
heartbeat_interval_secs = 30  # how often to call /heartbeat
```

Update the `## Runtime files` table to add:
```
| `STATE.lock` | Advisory lock file for atomic claiming |
```

- [ ] **Step 6: Verify compilation**

```sh
cargo build
```

- [ ] **Step 7: Full suite**

```sh
cargo test && cargo clippy -- -D warnings
```

- [ ] **Step 8: Commit**

```sh
git add src/cli/commands/init.rs
git commit -m "feat: create STATE.lock on init, add [lease] to template, update skill docs"
```

---

### Task 7: CLI flags — `--agent-name`/`--agent-index` for serve; optional `--supervisor`/`--executor` for register

**Files:**
- Modify: `src/cli/mod.rs`
- Modify: `src/cli/commands/serve.rs`

- [ ] **Step 1: Update `Serve` variant in `cli/mod.rs`**

Replace the `Serve` variant:

```rust
    /// Start the MCP server on stdio
    Serve {
        /// Filter the exposed tool set by role (omit to expose all tools)
        #[arg(long, value_enum)]
        role: Option<Role>,
        /// Human-readable agent name embedded in the claimed_by field (e.g. "codex", "claude-code")
        #[arg(long, default_value = "unknown")]
        agent_name: String,
        /// Index disambiguating multiple agents of the same role and name (e.g. 1, 2)
        #[arg(long, default_value_t = 0u32)]
        agent_index: u32,
    },
```

- [ ] **Step 2: Make `Register` flags optional in `cli/mod.rs`**

Replace the `Register` variant:

```rust
    /// Write MCP config files so agents can launch ferrus automatically
    Register {
        /// Agent to configure as Supervisor (optional if --executor is set)
        #[arg(long, value_enum, value_name = "AGENT")]
        supervisor: Option<commands::register::Agent>,
        /// Agent to configure as Executor (optional if --supervisor is set)
        #[arg(long, value_enum, value_name = "AGENT")]
        executor: Option<commands::register::Agent>,
    },
```

- [ ] **Step 3: Update dispatch in `Cli::run()`**

Replace the `Serve` and `Register` dispatch arms:

```rust
            Commands::Serve { role, agent_name, agent_index } => {
                commands::serve::run(role, agent_name, agent_index).await
            }
            Commands::Register { supervisor, executor } => {
                if supervisor.is_none() && executor.is_none() {
                    anyhow::bail!("At least one of --supervisor or --executor must be specified");
                }
                commands::register::run(supervisor, executor).await
            }
```

- [ ] **Step 4: Update `commands/serve.rs`**

Replace the entire file:

```rust
use anyhow::Result;

use crate::server::Role;

pub async fn run(role: Option<Role>, agent_name: String, agent_index: u32) -> Result<()> {
    crate::server::start(role, agent_name, agent_index).await
}
```

- [ ] **Step 5: Verify compilation (will fail at server::start signature mismatch — that's expected)**

```sh
cargo build 2>&1 | head -20
```
Expected: error about `server::start` argument count — proceed to Task 8.

---

### Task 8: `server/mod.rs` — agent_id, closure capture, heartbeat registration

**Files:**
- Modify: `src/server/mod.rs`

- [ ] **Step 1: Update `start()` signature and construct `agent_id`**

Replace the entire `src/server/mod.rs`:

```rust
use std::sync::Arc;

use anyhow::Result;
use neva::App;
use neva::types::ToolSchema;

mod prompts;
mod resources;
mod tools;

#[derive(Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Role {
    Supervisor,
    Executor,
}

pub async fn start(role: Option<Role>, agent_name: String, agent_index: u32) -> Result<()> {
    let role_str = match &role {
        Some(Role::Supervisor) => "supervisor",
        Some(Role::Executor) => "executor",
        None => "agent",
    };
    let agent_id = Arc::new(format!("{role_str}:{agent_name}:{agent_index}"));

    let mut app = App::new().with_options(|opt| {
        opt.with_stdio()
            .with_name("ferrus")
            .with_version(env!("CARGO_PKG_VERSION"))
    });

    let sup = role.as_ref().is_none_or(|r| *r == Role::Supervisor);
    let exe = role.as_ref().is_none_or(|r| *r == Role::Executor);

    if sup {
        app.map_tool("create_task", tools::create_task::handler)
            .with_description(tools::create_task::DESCRIPTION)
            .with_input_schema(|_| ToolSchema::from_json_str(tools::create_task::INPUT_SCHEMA));
        {
            let id = agent_id.clone();
            app.map_tool("wait_for_review", move || {
                let id = id.clone();
                async move { tools::wait_for_review::handler(&id).await }
            })
            .with_description(tools::wait_for_review::DESCRIPTION);
        }
        app.map_tool("review_pending", tools::review_pending::handler)
            .with_description(tools::review_pending::DESCRIPTION);
        app.map_tool("approve", tools::approve::handler)
            .with_description(tools::approve::DESCRIPTION);
        app.map_tool("reject", tools::reject::handler)
            .with_description(tools::reject::DESCRIPTION)
            .with_input_schema(|_| ToolSchema::from_json_str(tools::reject::INPUT_SCHEMA));
    }

    if exe {
        {
            let id = agent_id.clone();
            app.map_tool("wait_for_task", move || {
                let id = id.clone();
                async move { tools::wait_for_task::handler(&id).await }
            })
            .with_description(tools::wait_for_task::DESCRIPTION);
        }
        app.map_tool("next_task", tools::next_task::handler)
            .with_description(tools::next_task::DESCRIPTION);
        app.map_tool("check", tools::check::handler)
            .with_description(tools::check::DESCRIPTION);
        app.map_tool("submit", tools::submit::handler)
            .with_description(tools::submit::DESCRIPTION)
            .with_input_schema(|_| ToolSchema::from_json_str(tools::submit::INPUT_SCHEMA));
    }

    // Resources
    app.add_resource("ferrus://task", "Task");
    app.add_resource("ferrus://feedback", "Feedback");
    app.add_resource("ferrus://review", "Review Notes");
    app.add_resource("ferrus://submission", "Submission");
    app.add_resource("ferrus://question", "Question");
    app.add_resource("ferrus://state", "State");
    app.map_resource("ferrus://{file}", "ferrus-file", resources::read);

    // Prompts
    app.map_prompt("executor-context", prompts::executor_context)
        .with_description("Executor task context: state, task, feedback, and review notes");
    app.map_prompt("supervisor-review", prompts::supervisor_review)
        .with_description("Supervisor review context: state, task, and submission notes");

    // Shared tools (always registered regardless of role)
    app.map_tool("ask_human", tools::ask_human::handler)
        .with_description(tools::ask_human::DESCRIPTION)
        .with_input_schema(|_| ToolSchema::from_json_str(tools::ask_human::INPUT_SCHEMA));
    app.map_tool("answer", tools::answer::handler)
        .with_description(tools::answer::DESCRIPTION)
        .with_input_schema(|_| ToolSchema::from_json_str(tools::answer::INPUT_SCHEMA));
    app.map_tool("status", tools::status::handler)
        .with_description(tools::status::DESCRIPTION);
    app.map_tool("reset", tools::reset::handler)
        .with_description(tools::reset::DESCRIPTION);
    {
        let id = agent_id.clone();
        app.map_tool("heartbeat", move |agent_id: String| {
            let id = id.clone();
            async move { tools::heartbeat::handler(&id, agent_id).await }
        })
        .with_description(tools::heartbeat::DESCRIPTION)
        .with_input_schema(|_| ToolSchema::from_json_str(tools::heartbeat::INPUT_SCHEMA));
    }

    app.run().await;
    Ok(())
}
```

- [ ] **Step 2: Add `pub mod heartbeat` to `tools/mod.rs`**

In `src/server/tools/mod.rs`, add:

```rust
pub mod heartbeat;
```

(alphabetical order — after `check`, before `next_task`)

- [ ] **Step 3: Create stub `heartbeat.rs` so the crate compiles**

Create `src/server/tools/heartbeat.rs`:

```rust
use anyhow::Result;
use neva::prelude::*;

use super::tool_err;

pub const DESCRIPTION: &str = "Renew the lease for the calling agent. \
    Validates that the agent holds the current lease, then extends lease_until \
    and updates last_heartbeat. Returns a JSON object with status \"renewed\" on \
    success or status \"error\" with a code on failure. \
    Error codes: not_claimed, claimed_by_other, expired, invalid_state.";

pub const INPUT_SCHEMA: &str = r#"{
    "properties": {
        "agent_id": {
            "type": "string",
            "description": "The caller's agent identifier, e.g. \"executor:codex:1\""
        }
    },
    "required": ["agent_id"]
}"#;

pub async fn handler(_server_agent_id: &str, _agent_id: String) -> Result<String, Error> {
    // Implemented in Task 12
    Err(tool_err(anyhow::anyhow!("not yet implemented")))
}
```

- [ ] **Step 4: Verify it compiles**

```sh
cargo build
```
Expected: compiles cleanly.

- [ ] **Step 5: Run full suite**

```sh
cargo test && cargo clippy -- -D warnings
```

- [ ] **Step 6: Commit**

```sh
git add src/server/mod.rs src/server/tools/mod.rs src/server/tools/heartbeat.rs src/cli/mod.rs src/cli/commands/serve.rs
git commit -m "feat: agent_id wiring — --agent-name/--agent-index flags, closure capture, heartbeat stub"
```

---

### Task 9: `register.rs` — `Agent::name()`, optional flags, index counting, indexed keys

**Files:**
- Modify: `src/cli/commands/register.rs`

- [ ] **Step 1: Add `Agent::name()` helper**

Add after the `Agent` enum:

```rust
impl Agent {
    /// The string representation used in --agent-name CLI flags and claimed_by identifiers.
    pub fn name(&self) -> &str {
        match self {
            Agent::ClaudeCode => "claude-code",
            Agent::Codex => "codex",
        }
    }
}
```

- [ ] **Step 2: Update `run()` signature**

Change to:

```rust
pub async fn run(supervisor: Option<Agent>, executor: Option<Agent>) -> Result<()> {
```

- [ ] **Step 3: Restructure `run()` body**

Replace the body of `run()`:

```rust
pub async fn run(supervisor: Option<Agent>, executor: Option<Agent>) -> Result<()> {
    if let Some(agent) = &supervisor {
        register_role("supervisor", agent).await?;
    }
    if let Some(agent) = &executor {
        register_role("executor", agent).await?;
    }
    Ok(())
}

async fn register_role(role: &str, agent: &Agent) -> Result<()> {
    let agent_name = agent.name();
    match agent {
        Agent::ClaudeCode => register_claude_code(role, agent_name).await,
        Agent::Codex => register_codex(role, agent_name).await,
    }
}
```

- [ ] **Step 4: Rewrite `write_claude_code` as `register_claude_code`**

Replace `write_claude_code` with:

```rust
async fn register_claude_code(role: &str, agent_name: &str) -> Result<()> {
    let path = std::path::Path::new(".mcp.json");

    let mut root: serde_json::Value = if path.exists() {
        let content = tokio::fs::read_to_string(path).await?;
        serde_json::from_str(&content).unwrap_or(serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    let servers = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!(".mcp.json root is not a JSON object"))?
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));

    let servers_obj = servers
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!(".mcp.json mcpServers is not a JSON object"))?;

    let index = count_mcp_entries(servers_obj, role, agent_name) + 1;
    let key = format!("ferrus-{role}-{index}");

    servers_obj.insert(
        key.clone(),
        serde_json::json!({
            "command": "ferrus",
            "args": ["serve", "--role", role, "--agent-name", agent_name, "--agent-index", index.to_string()]
        }),
    );
    println!("Registered {key} in .mcp.json (agent_id will be \"{role}:{agent_name}:{index}\")");

    let content = serde_json::to_string_pretty(&root)?;
    tokio::fs::write(path, content).await?;
    Ok(())
}

/// Count existing entries in `mcpServers` whose args contain both
/// `--role <role>` and `--agent-name <agent_name>`.
fn count_mcp_entries(
    servers: &serde_json::Map<String, serde_json::Value>,
    role: &str,
    agent_name: &str,
) -> u32 {
    servers.values().filter(|entry| {
        let args = match entry.get("args").and_then(|a| a.as_array()) {
            Some(a) => a,
            None => return false,
        };
        let strings: Vec<&str> = args.iter()
            .filter_map(|v| v.as_str())
            .collect();
        has_flag_pair(&strings, "--role", role)
            && has_flag_pair(&strings, "--agent-name", agent_name)
    }).count() as u32
}
```

- [ ] **Step 5: Rewrite `write_codex` as `register_codex`**

Replace `write_codex` with:

```rust
async fn register_codex(role: &str, agent_name: &str) -> Result<()> {
    let dir = std::path::Path::new(".codex");
    tokio::fs::create_dir_all(dir).await?;
    let path = dir.join("config.toml");

    let mut table: toml::Table = if path.exists() {
        let content = tokio::fs::read_to_string(&path).await?;
        content.parse()?
    } else {
        toml::Table::new()
    };

    let mcp_servers = table
        .entry("mcp_servers")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!(".codex/config.toml mcp_servers is not a table"))?;

    let index = count_codex_entries(mcp_servers, role, agent_name) + 1;
    let key = format!("ferrus-{role}-{index}");

    let mut entry = toml::Table::new();
    entry.insert("command".to_string(), toml::Value::String("ferrus".to_string()));
    entry.insert(
        "args".to_string(),
        toml::Value::Array(vec![
            toml::Value::String("serve".to_string()),
            toml::Value::String("--role".to_string()),
            toml::Value::String(role.to_string()),
            toml::Value::String("--agent-name".to_string()),
            toml::Value::String(agent_name.to_string()),
            toml::Value::String("--agent-index".to_string()),
            toml::Value::String(index.to_string()),
        ]),
    );
    mcp_servers.insert(key.clone(), toml::Value::Table(entry));
    println!("Registered {key} in .codex/config.toml (agent_id will be \"{role}:{agent_name}:{index}\")");

    let content = toml::to_string_pretty(&table)?;
    tokio::fs::write(&path, content).await?;
    Ok(())
}

/// Count existing entries in `mcp_servers` whose args array contains both
/// `--role <role>` and `--agent-name <agent_name>`.
fn count_codex_entries(servers: &toml::Table, role: &str, agent_name: &str) -> u32 {
    servers.values().filter(|entry| {
        let args = match entry.get("args").and_then(|v| v.as_array()) {
            Some(a) => a,
            None => return false,
        };
        let strings: Vec<&str> = args.iter()
            .filter_map(|v| v.as_str())
            .collect();
        has_flag_pair(&strings, "--role", role)
            && has_flag_pair(&strings, "--agent-name", agent_name)
    }).count() as u32
}

/// Returns true if `args` contains `flag` immediately followed by `value`.
fn has_flag_pair(args: &[&str], flag: &str, value: &str) -> bool {
    args.windows(2).any(|w| w[0] == flag && w[1] == value)
}
```

- [ ] **Step 6: Build and verify**

```sh
cargo build
```
Expected: compiles. Verify manually with `ferrus register --executor codex` if a `.ferrus/` dir exists.

- [ ] **Step 7: Full suite**

```sh
cargo test && cargo clippy -- -D warnings
```

- [ ] **Step 8: Commit**

```sh
git add src/cli/commands/register.rs
git commit -m "feat: register --agent-name/--agent-index, indexed keys, index auto-counting"
```

---

## Chunk 3: Tools — Wait, Heartbeat, Final Wire-up

### Task 10: `wait_for_task.rs` — file-lock claim loop, structured JSON

**Files:**
- Modify: `src/server/tools/wait_for_task.rs`

- [ ] **Step 1: Replace the implementation**

Replace the entire file:

```rust
use anyhow::Result;
use fs2::FileExt;
use neva::prelude::*;
use serde_json::json;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::info;

use crate::{
    config::Config,
    state::{machine::TaskState, store},
};

use super::tool_err;

pub const DESCRIPTION: &str =
    "Block until a task is ready to work on, then atomically claim it and return its full context. \
     Returns a JSON object: {\"status\":\"claimed\", \"claimed_by\":\"...\", \"lease_until\":\"...\", \
     \"state\":\"...\", \"task\":\"...\", \"feedback\":\"...\", \"review\":\"...\"} when a task is \
     claimed, or {\"status\":\"timeout\", \"state\":\"...\"} on timeout. \
     On timeout, inspect the state field — call wait_for_task again only if the state is \
     Executing or Addressing. \
     Times out after `wait_timeout_secs` (see ferrus.toml). \
     Call this at startup and after each /submit to form an autonomous loop.";

pub async fn handler(agent_id: &str) -> Result<String, Error> {
    run(agent_id).await.map_err(tool_err)
}

async fn run(agent_id: &str) -> Result<String> {
    let config = Config::load().await?;
    let timeout = Duration::from_secs(config.limits.wait_timeout_secs);
    let ttl_secs = config.lease.ttl_secs;
    let start = Instant::now();

    loop {
        // Acquire exclusive advisory lock, read state, conditionally claim — all atomically.
        let (claimed, _) = {
            let lock_file = store::open_lock_file()?;
            // Blocking call: run off the async thread so we don't block the runtime.
            let lock_file = tokio::task::spawn_blocking(move || -> Result<std::fs::File> {
                lock_file.lock_exclusive().map_err(anyhow::Error::from)?;
                Ok(lock_file)
            })
            .await??;

            let mut state = store::read_state().await?;

            let claimable = matches!(state.state, TaskState::Executing | TaskState::Addressing);
            let claimed = if claimable && !state.is_claimed() {
                store::claim_state(agent_id, ttl_secs, &mut state).await?;
                true
            } else if claimable && state.is_claimed_by(agent_id) {
                // Idempotent re-entry: this agent already holds the lease.
                true
            } else {
                false
            };

            // Release lock by dropping lock_file.
            drop(lock_file);
            (claimed, state)
        };

        if claimed {
            let task = store::read_task().await?;
            let feedback = store::read_feedback().await?;
            let review = store::read_review().await?;

            // Re-read state to get the stamped lease_until.
            let state = store::read_state().await?;

            info!(agent_id, "Executor claimed task");
            let response = json!({
                "status": "claimed",
                "claimed_by": state.claimed_by,
                "lease_until": state.lease_until,
                "state": format!("{:?}", state.state),
                "task": task,
                "feedback": feedback,
                "review": review,
            });
            return Ok(response.to_string());
        }

        if start.elapsed() >= timeout {
            let state = store::read_state().await?;
            info!("wait_for_task timed out, state: {:?}", state.state);
            let response = json!({
                "status": "timeout",
                "state": format!("{:?}", state.state),
            });
            return Ok(response.to_string());
        }

        sleep(Duration::from_millis(500)).await;
    }
}
```

- [ ] **Step 2: Build**

```sh
cargo build
```

- [ ] **Step 3: Full suite**

```sh
cargo test && cargo clippy -- -D warnings
```

- [ ] **Step 4: Commit**

```sh
git add src/server/tools/wait_for_task.rs
git commit -m "feat: wait_for_task — file-lock atomic claim, structured JSON response"
```

---

### Task 11: `wait_for_review.rs` — same pattern for Supervisor

**Files:**
- Modify: `src/server/tools/wait_for_review.rs`

- [ ] **Step 1: Replace the implementation**

Replace the entire file:

```rust
use anyhow::Result;
use fs2::FileExt;
use neva::prelude::*;
use serde_json::json;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::info;

use crate::{
    config::Config,
    state::{machine::TaskState, store},
};

use super::tool_err;

pub const DESCRIPTION: &str =
    "Block until the Executor submits work for review, then atomically claim the review and \
     return the full submission context. \
     Returns a JSON object: {\"status\":\"claimed\", \"claimed_by\":\"...\", \"lease_until\":\"...\", \
     \"state\":\"Reviewing\", \"task\":\"...\", \"submission\":\"...\", \"feedback\":\"...\", \"review\":\"...\"} \
     when a submission is ready, or {\"status\":\"timeout\", \"state\":\"...\"} on timeout. \
     Times out after `wait_timeout_secs` (see ferrus.toml). \
     Returns immediately if a submission is already pending — safe to call on restart.";

pub async fn handler(agent_id: &str) -> Result<String, Error> {
    run(agent_id).await.map_err(tool_err)
}

async fn run(agent_id: &str) -> Result<String> {
    let config = Config::load().await?;
    let timeout = Duration::from_secs(config.limits.wait_timeout_secs);
    let ttl_secs = config.lease.ttl_secs;
    let start = Instant::now();

    loop {
        let (claimed, _) = {
            let lock_file = store::open_lock_file()?;
            let lock_file = tokio::task::spawn_blocking(move || -> Result<std::fs::File> {
                lock_file.lock_exclusive().map_err(anyhow::Error::from)?;
                Ok(lock_file)
            })
            .await??;

            let mut state = store::read_state().await?;

            let claimable = state.state == TaskState::Reviewing;
            let claimed = if claimable && !state.is_claimed() {
                store::claim_state(agent_id, ttl_secs, &mut state).await?;
                true
            } else if claimable && state.is_claimed_by(agent_id) {
                true
            } else {
                false
            };

            drop(lock_file);
            (claimed, state)
        };

        if claimed {
            let task = store::read_task().await?;
            let submission = store::read_submission().await?;
            let feedback = store::read_feedback().await?;
            let review = store::read_review().await?;
            let state = store::read_state().await?;

            info!(agent_id, "Supervisor claimed review");
            let response = json!({
                "status": "claimed",
                "claimed_by": state.claimed_by,
                "lease_until": state.lease_until,
                "state": format!("{:?}", state.state),
                "task": task,
                "submission": submission,
                "feedback": feedback,
                "review": review,
                "review_cycles_used": state.review_cycles,
                "check_retries_used": state.check_retries,
            });
            return Ok(response.to_string());
        }

        if start.elapsed() >= timeout {
            let state = store::read_state().await?;
            info!("wait_for_review timed out, state: {:?}", state.state);
            let response = json!({
                "status": "timeout",
                "state": format!("{:?}", state.state),
            });
            return Ok(response.to_string());
        }

        sleep(Duration::from_millis(500)).await;
    }
}
```

- [ ] **Step 2: Build + full suite**

```sh
cargo build && cargo test && cargo clippy -- -D warnings
```

- [ ] **Step 3: Commit**

```sh
git add src/server/tools/wait_for_review.rs
git commit -m "feat: wait_for_review — file-lock atomic claim, structured JSON response"
```

---

### Task 12: `heartbeat.rs` — full implementation

**Files:**
- Modify: `src/server/tools/heartbeat.rs`

- [ ] **Step 1: Replace the stub with the full implementation**

Replace the entire file:

```rust
use anyhow::Result;
use fs2::FileExt;
use neva::prelude::*;
use serde_json::json;
use tracing::info;

use crate::{
    config::Config,
    state::{machine::TaskState, store},
};

use super::tool_err;

pub const DESCRIPTION: &str =
    "Renew the lease for the calling agent. Validates that the agent holds the current lease, \
     then extends lease_until by ttl_secs and updates last_heartbeat. \
     Returns a JSON object: {\"status\":\"renewed\", \"claimed_by\":\"...\", \"lease_until\":\"...\"} \
     on success, or {\"status\":\"error\", \"code\":\"...\", \"message\":\"...\"} on failure. \
     Error codes: not_claimed (no active lease), claimed_by_other (different agent holds lease), \
     expired (your lease timed out before renewal), invalid_state (state cannot be leased).";

pub const INPUT_SCHEMA: &str = r#"{
    "properties": {
        "agent_id": {
            "type": "string",
            "description": "The caller's agent identifier, e.g. \"executor:codex:1\""
        }
    },
    "required": ["agent_id"]
}"#;

const LEASABLE_STATES: &[TaskState] = &[
    TaskState::Executing,
    TaskState::Addressing,
    TaskState::Checking,
    TaskState::Reviewing,
];

pub async fn handler(server_agent_id: &str, agent_id: String) -> Result<String, Error> {
    run(server_agent_id, &agent_id).await.map_err(tool_err)
}

async fn run(_server_agent_id: &str, agent_id: &str) -> Result<String> {
    let config = Config::load().await?;
    let ttl_secs = config.lease.ttl_secs;

    // Acquire lock for the full read-validate-write cycle.
    let lock_file = store::open_lock_file()?;
    let lock_file = tokio::task::spawn_blocking(move || -> Result<std::fs::File> {
        lock_file.lock_exclusive().map_err(anyhow::Error::from)?;
        Ok(lock_file)
    })
    .await??;

    let mut state = store::read_state().await?;

    // Step 1: identity check (ignoring expiry) — determines not_claimed vs claimed_by_other.
    let identity_match = state.claimed_by.as_deref() == Some(agent_id);
    if !identity_match {
        drop(lock_file);
        return Ok(if state.claimed_by.is_none() {
            json!({
                "status": "error",
                "code": "not_claimed",
                "message": "No active lease exists"
            })
        } else {
            json!({
                "status": "error",
                "code": "claimed_by_other",
                "message": format!("Lease is held by {}", state.claimed_by.as_deref().unwrap_or("unknown"))
            })
        }.to_string());
    }

    // Step 2: expiry check — fires when this agent's lease has already timed out.
    if state.lease_expired() {
        drop(lock_file);
        return Ok(json!({
            "status": "error",
            "code": "expired",
            "message": "Your lease expired before renewal"
        }).to_string());
    }

    // Step 3: state must be leasable.
    if !LEASABLE_STATES.contains(&state.state) {
        drop(lock_file);
        return Ok(json!({
            "status": "error",
            "code": "invalid_state",
            "message": format!("State {:?} cannot hold a lease", state.state)
        }).to_string());
    }

    // Renew the lease. `claim_state` mutates `state` in place (sets claimed_by,
    // lease_until, last_heartbeat on the &mut reference) before writing to disk,
    // so `state.lease_until` below reflects the renewed value without a second read.
    store::claim_state(agent_id, ttl_secs, &mut state).await?;
    drop(lock_file);

    let renewed_until = state.lease_until;
    info!(agent_id, "Lease renewed");

    Ok(json!({
        "status": "renewed",
        "claimed_by": agent_id,
        "lease_until": renewed_until,
    }).to_string())
}
```

- [ ] **Step 2: Add `PartialEq` to `TaskState` for `LEASABLE_STATES.contains()`**

`TaskState` already derives `PartialEq` — verify this is present in `machine.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskState { ... }
```

No change needed if it's already there.

- [ ] **Step 3: Build + full suite**

```sh
cargo build && cargo test && cargo clippy -- -D warnings
```

- [ ] **Step 4: Commit**

```sh
git add src/server/tools/heartbeat.rs
git commit -m "feat: /heartbeat tool — lease renewal with identity/expiry/state validation"
```

---

### Task 13: Final verification

- [ ] **Step 1: Run the full build, test, lint pipeline**

```sh
cargo build --release && cargo test && cargo clippy -- -D warnings && cargo fmt --check
```
Expected: all pass.

- [ ] **Step 2: Smoke-test the CLI flags**

```sh
cargo run -- serve --help
```
Expected: output shows `--agent-name` and `--agent-index` options.

```sh
cargo run -- register --help
```
Expected: output shows `--supervisor` and `--executor` as optional `[AGENT]`.

- [ ] **Step 3: Final commit**

```sh
git add -p   # stage any remaining unstaged changes
git commit -m "chore: final lint and formatting pass"
```
(Skip if nothing to commit.)
