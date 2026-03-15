# Lease & Heartbeat Foundation Design

**Date:** 2026-03-15
**Status:** Approved
**Scope:** `STATE.json` lease fields, atomic claiming in `wait_for_*`, `/heartbeat` tool, `ferrus register` index auto-assignment, `[lease]` config block

---

## Problem

The current `wait_for_task` polls `STATE.json` and returns as soon as it sees `Executing` or `Addressing`, with no record of which agent picked up the task. Two executors polling simultaneously can both read the same state and both believe they own the task. There is also no way to detect a crashed agent holding an uncompleted task.

This design adds a lease-based ownership layer as a foundation for multi-executor and multi-supervisor support.

---

## Goals

- Add `claimed_by`, `lease_until`, `last_heartbeat` to `STATE.json`
- Make `wait_for_task` and `wait_for_review` atomically claim tasks via file locking
- Add a `/heartbeat` tool for lease renewal
- Emit structured JSON from `wait_for_*` so agents can act on `"timeout"` vs `"claimed"`
- Introduce a `[lease]` block in `ferrus.toml` with sensible defaults
- Update `ferrus register` to bake `--agent-name` and `--agent-index` into the generated `args`
- Add `--agent-name` and `--agent-index` CLI flags to `ferrus serve`

---

## Non-Goals

- Multiple concurrent tasks (still one task at a time per `.ferrus/` directory)
- Automatic eviction or task reassignment (deferred — lease expiry is detected opportunistically)
- Separate lease files per agent (deferred — single `STATE.json` is sufficient for this foundation)

---

## Configuration

New `[lease]` block in `ferrus.toml`:

```toml
[lease]
ttl_secs = 90
heartbeat_interval_secs = 30
```

`ttl_secs` — how long a lease is valid without renewal. Default: 90.
`heartbeat_interval_secs` — how often agents should call `/heartbeat`. Default: 30. Informational — used in skill file guidance, not enforced server-side.

The 3:1 ratio (TTL = 3× heartbeat interval) means an agent must miss three consecutive heartbeats before its lease expires.

`LeaseConfig` is added to `config/mod.rs` with `#[serde(default)]` so existing `ferrus.toml` files without the block continue to work.

The `DEFAULT_FERRUS_TOML` constant in `src/cli/commands/init.rs` gains:

```toml
[lease]
ttl_secs = 90
heartbeat_interval_secs = 30
```

---

## STATE.json — New Fields

```rust
pub claimed_by: Option<String>,          // e.g. "executor:codex:1"
pub lease_until: Option<DateTime<Utc>>,  // claimed_at + ttl_secs
pub last_heartbeat: Option<DateTime<Utc>>,
```

All three fields use `#[serde(default, skip_serializing_if = "Option::is_none")]`. Absent when unclaimed — existing `STATE.json` files deserialize cleanly with no schema version bump required.

### Helpers on `StateData`

```rust
/// True if a non-expired lease exists (lease_until is set and in the future).
pub fn is_claimed(&self) -> bool {
    self.lease_until.map_or(false, |t| Utc::now() < t)
}

/// True if this specific agent holds a valid (non-expired) lease.
pub fn is_claimed_by(&self, agent_id: &str) -> bool {
    self.claimed_by.as_deref() == Some(agent_id) && self.is_claimed()
}

/// True when lease_until is None or has been reached/passed.
/// Returns true for unclaimed state (None) so the claim check
/// in wait_for_task is simply `!state.is_claimed()`.
pub fn lease_expired(&self) -> bool {
    self.lease_until.map_or(true, |t| Utc::now() >= t)
}
```

### Invariants

All state-mutating tools pass the existing `StateData` through `write_state` via `..state.clone()`, so lease fields survive every tool call automatically. Implementers must not reconstruct `StateData::default()` mid-tool.

The one intentional exception is `/reset`: `state.reset()` calls `*self = Self::default()`, which sets all `Option` lease fields to `None`. This is correct — a reset means no agent is active and the slate is clean. This behaviour is intentional and does not need special handling.

---

## Lease Ownership by Phase

| State | Lease owner |
|---|---|
| `Executing`, `Addressing`, `Checking` | Executor |
| `Reviewing` | Supervisor |
| `Complete`, `Failed`, `Idle` | None — must always be unclaimed |

On every transition between Executor-owned and Supervisor-owned phases the existing lease must be cleared before the next side can claim. A Supervisor must never find an Executor's lease still set when it tries to claim `Reviewing`, and vice versa.

---

## Lease Reset on Transitions

The following tools must clear `claimed_by`, `lease_until`, and `last_heartbeat` (set all to `None`) before writing the new state:

| Tool | Transition | Reason |
|---|---|---|
| `/submit` | `Checking → Reviewing` | Hands off from Executor to Supervisor |
| `/reject` | `Reviewing → Addressing` | Hands off from Supervisor back to Executor |
| `/approve` | `Reviewing → Complete` | Task finished — no owner needed |
| `/reset` | `Failed → Idle` | Already handled by `StateData::default()` in `reset()` |
| `/create_task` | `Idle → Executing` | Ensures a new task always starts unclaimed |

`/check` does **not** clear the lease — it transitions within the Executor's owned phase (`Executing`/`Addressing` → `Checking`) and the same Executor remains the claimant throughout.

---

## File Locking Strategy

Atomic claiming uses a dedicated lock file `.ferrus/STATE.lock` rather than locking `STATE.json` directly. This avoids a race condition: `write_state` writes to `STATE.json.tmp` then renames it to `STATE.json`, which replaces the inode. Any waiter that acquired an `flock` on the old `STATE.json` inode would be holding a lock on a file that no longer exists at that path, undermining mutual exclusion.

Using `STATE.lock` as the lock file:
- Its inode is stable across `STATE.json` renames
- The lock is held only for the read-check-write cycle
- `STATE.json` itself continues to be written via atomic rename as before

Locking is implemented via the `fs2` crate (`fs2 = "0.4"`). `STATE.lock` is created by `ferrus init`. It is gitignored via the `.ferrus/` directory rule already present in `.gitignore`.

**All tools that read then write `STATE.json` must acquire `STATE.lock` first** — not just `wait_for_*`, but also `/heartbeat`. This ensures no write races between a claim in progress and a concurrent lease renewal.

```rust
// Pseudocode — acquire lock, check state, conditionally write claim, release
let lock_file = File::open(".ferrus/STATE.lock")?;
lock_file.lock_exclusive()?;   // fs2::FileExt
let state = read_state().await?;
if /* claim condition */ {
    write_claim(&mut state, agent_id, ttl).await?;
}
lock_file.unlock()?;
```

---

## Atomic Claim in `wait_for_task` / `wait_for_review`

Both tools acquire the `STATE.lock` lock on each iteration. The lock is held only for the read-check-write cycle.

### `wait_for_task` poll loop

```
loop:
  acquire exclusive flock on STATE.lock
  read STATE.json
  if state == Executing or Addressing:
    if !state.is_claimed():                               // unclaimed or expired
      write claim: claimed_by=agent_id, lease_until=now+ttl, last_heartbeat=now
      release lock
      read task, feedback, review
      return {"status":"claimed", "claimed_by":..., "lease_until":..., "state":...,
              "task":..., "feedback":..., "review":...}
    if state.is_claimed_by(agent_id):                    // idempotent re-entry
      release lock
      read task, feedback, review
      return {"status":"claimed", "claimed_by":..., "lease_until":..., "state":...,
              "task":..., "feedback":..., "review":...}
  release lock
  if elapsed >= timeout:
    return {"status":"timeout", "state": <current state>}
  sleep 500ms
```

Both the new-claim and idempotent-re-entry paths read and return the full task context (task, feedback, review) after releasing the lock. File reads for `TASK.md`, `FEEDBACK.md`, and `REVIEW.md` happen outside the lock — this is safe because those files are only written by tools that themselves hold the lock or operate outside the claim lifecycle.

`wait_for_review` follows the same pattern but checks for `Reviewing` state and is used by the Supervisor role.

---

## Structured Responses

### Claimed

```json
{
  "status": "claimed",
  "claimed_by": "executor:codex:1",
  "lease_until": "2026-03-15T10:01:30Z",
  "state": "Executing",
  "task": "...",
  "feedback": "...",
  "review": "..."
}
```

`feedback` and `review` are empty string when not applicable.

### Timeout

```json
{
  "status": "timeout",
  "state": "Idle"
}
```

The agent should inspect the `"state"` field before looping. A timeout does not imply `Idle` — the state may be `Complete`, `Failed`, `Reviewing`, or any other value. Agents must handle non-actionable states gracefully (e.g. log and retry, or surface to the user) rather than assuming they should call `wait_for_task` again unconditionally.

---

## `/heartbeat` Tool

Available to both Supervisor and Executor roles.

### Input Schema

```json
{
  "type": "object",
  "properties": {
    "agent_id": {
      "type": "string",
      "description": "The caller's agent identifier, e.g. \"executor:codex:1\""
    }
  },
  "required": ["agent_id"]
}
```

### Validation Logic

The tool validates in this order to produce distinct, reachable error codes:

1. Check `state.claimed_by == Some(agent_id)` (identity check, ignoring expiry)
   - If `None` → `not_claimed`
   - If `Some(other)` → `claimed_by_other`
2. Check `!state.lease_expired()` (expiry check, only for the matched agent)
   - If expired → `expired`
3. Check state is in a leasable state (Executing, Addressing, Checking, Reviewing)
   - Otherwise → `invalid_state`
4. Write renewal: `last_heartbeat = now`, `lease_until = now + ttl_secs`

This ordering makes all four error codes reachable. In particular, `expired` fires when the correct agent attempts renewal but has already missed the TTL window (and no other agent has yet claimed it — `claimed_by` still matches but `lease_until` is in the past).

### Success Response

```json
{
  "status": "renewed",
  "claimed_by": "executor:codex:1",
  "lease_until": "2026-03-15T10:02:00Z"
}
```

### Error Response

```json
{
  "status": "error",
  "code": "<code>",
  "message": "<human-readable description>"
}
```

### Error Codes

| Code | Meaning |
|---|---|
| `not_claimed` | No agent holds the lease (`claimed_by` is absent) |
| `claimed_by_other` | Lease is held by a different agent (message names them) |
| `expired` | Caller's own lease timed out before renewal (but no other agent has yet claimed it) |
| `invalid_state` | State is not in a leasable state (e.g. Idle, Complete, Failed) |

The `/heartbeat` tool does **not** change `TaskState` — it only refreshes the lease timestamps. It acquires `STATE.lock` for the full read-validate-write cycle, same as `wait_for_*`.

---

## `agent_id` Propagation to Tool Handlers

The existing tool handler signature for tools without parameters is `pub async fn handler() -> Result<String, Error>`. The `agent_id` string is not an MCP input for `wait_for_task` or `wait_for_review` — it is derived from CLI flags at server startup and captured via closure at registration time in `server/mod.rs`:

```rust
let agent_id = Arc::new(format!("{}:{}:{}", role_str, agent_name, agent_index));

app.map_tool(
    ToolSchema { name: "wait_for_task", ... },
    {
        let id = agent_id.clone();
        move |_ctx: Context<()>| {
            let id = id.clone();
            async move { wait_for_task::handler(id.as_str()).await }
        }
    },
);
```

Tool handler signatures for `wait_for_task`, `wait_for_review`, and `heartbeat` become:

```rust
pub async fn handler(agent_id: &str) -> Result<String, Error>
```

The `heartbeat` handler additionally reads `agent_id` from its MCP input params (for the `agent_id` field the caller provides) but also receives the server-side `agent_id` via closure for cross-validation if needed.

---

## `ferrus register` — Index Auto-Assignment

`ferrus register` gains the ability to count existing entries and auto-assign `--agent-index`.

### Example

```sh
ferrus register --executor codex        # → executor:codex:1
ferrus register --executor codex        # → executor:codex:2
ferrus register --supervisor claude-code # → supervisor:claude-code:1
```

For each role+name pair, `ferrus register`:
1. Reads the existing MCP config (`.mcp.json` or `.codex/config.toml`)
2. Deserializes entries structurally (not text matching) and counts entries whose `args` array contains both `--role <role>` and `--agent-name <name>` as consecutive or paired elements
3. Assigns `--agent-index <count+1>`
4. Writes a new entry with key `ferrus-{role}-{index}` (e.g. `ferrus-executor-1`, `ferrus-executor-2`)

The key format `ferrus-{role}-{index}` avoids key collisions when multiple agents of the same role are registered. Existing single-agent registrations (key `ferrus-executor`) are not renamed — the new scheme applies only to entries written after this change.

Generated `args` shape:

```json
["serve", "--role", "executor", "--agent-name", "codex", "--agent-index", "1"]
```

---

## `ferrus serve` — New CLI Flags

```sh
ferrus serve --role executor --agent-name codex --agent-index 1
```

`--agent-name` and `--agent-index` are optional. Defaults: `agent_name = "unknown"`, `agent_index = 0`. This keeps single-agent setups working without any config change; `claimed_by` will be `"executor:unknown:0"` in that case.

At startup, the server constructs:

```rust
let agent_id = format!("{}:{}:{}", role_str, agent_name, agent_index);
```

---

## Skill File Updates

- `ferrus-executor/SKILL.md` — document the heartbeat loop: call `/heartbeat` every ~`heartbeat_interval_secs` while working; re-call `wait_for_task` on `"timeout"` after inspecting the `"state"` field
- `ferrus-supervisor/SKILL.md` — same for `wait_for_review` + `/heartbeat`
- `ferrus/SKILL.md` — add `/heartbeat` to the shared tools table; add lease fields to STATE.json reference; add `[lease]` to `ferrus.toml` example

---

## Files Changed

| File | Change |
|---|---|
| `Cargo.toml` | Add `fs2 = "0.4"` dependency |
| `src/config/mod.rs` | Add `LeaseConfig`, `#[serde(default)]` field on `Config` |
| `src/state/machine.rs` | Add 3 fields + 3 helpers to `StateData` |
| `src/state/store.rs` | Add `claim_state(agent_id, ttl)` pure write helper (sets 3 lease fields + calls `write_state`; callers hold the lock); create `STATE.lock` on init path |
| `src/server/tools/wait_for_task.rs` | `STATE.lock` file-lock claim loop, structured JSON output, `agent_id: &str` param |
| `src/server/tools/wait_for_review.rs` | `STATE.lock` file-lock claim loop, structured JSON output, `agent_id: &str` param |
| `src/server/tools/heartbeat.rs` | New tool: renewal + structured errors + INPUT_SCHEMA |
| `src/server/tools/mod.rs` | Add `pub mod heartbeat` |
| `src/server/mod.rs` | Construct `agent_id`; pass via closure capture to wait + heartbeat tools; register `/heartbeat` in the shared (unconditional) block alongside `ask_human`, `answer`, `status`, `reset` |
| `src/cli/commands/serve.rs` | Add `--agent-name`, `--agent-index` flags; construct `agent_id` string |
| `src/cli/commands/register.rs` | Structural count of existing entries; write `--agent-name`/`--agent-index`; use `ferrus-{role}-{index}` key format |
| `src/cli/commands/init.rs` | Add `[lease]` block to `DEFAULT_FERRUS_TOML`; create `.ferrus/STATE.lock` on init |
| Skill files (3×) | Document heartbeat loop, new tool, new config, new STATE fields |
