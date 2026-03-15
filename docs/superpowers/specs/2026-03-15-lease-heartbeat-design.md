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
`heartbeat_interval_secs` — how often agents should call `/heartbeat`. Default: 30. (Informational for skill files; not enforced by the server.)

The 3:1 ratio (TTL = 3× heartbeat interval) means an agent must miss three consecutive heartbeats before its lease expires.

`LeaseConfig` is added to `config/mod.rs` with `#[serde(default)]` so existing `ferrus.toml` files without the block continue to work.

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
/// True if a non-expired lease exists.
pub fn is_claimed(&self) -> bool {
    self.lease_until.map_or(false, |t| Utc::now() < t)
}

/// True if this agent holds a valid (non-expired) lease.
pub fn is_claimed_by(&self, agent_id: &str) -> bool {
    self.claimed_by.as_deref() == Some(agent_id) && self.is_claimed()
}

/// True when lease_until is None or has been reached/passed.
pub fn lease_expired(&self) -> bool {
    self.lease_until.map_or(true, |t| Utc::now() >= t)
}
```

`lease_expired()` returns `true` for unclaimed state (`None`) — so the claim check in `wait_for_task` is simply `!state.is_claimed()`.

---

## Atomic Claim in `wait_for_task` / `wait_for_review`

Both tools use `fs2` for advisory file locking. The lock is held only for the read-check-write cycle.

### `wait_for_task` poll loop

```
loop:
  acquire exclusive flock on STATE.json
  read state
  if state == Executing or Addressing:
    if !state.is_claimed() or state.lease_expired():
      write claim: claimed_by = agent_id, lease_until = now + ttl, last_heartbeat = now
      release lock
      return {"status":"claimed", ...}
    if state.is_claimed_by(agent_id):
      release lock
      return {"status":"claimed", ...}   // idempotent re-entry
  release lock
  if elapsed >= timeout:
    return {"status":"timeout", "state": <current state>}
  sleep 500ms
```

`wait_for_review` follows the same pattern but checks for `Reviewing` state and is used by the Supervisor role.

### Dependency

Add `fs2 = "0.4"` to `Cargo.toml`.

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

`feedback` and `review` are omitted or empty string when not applicable.

### Timeout

```json
{
  "status": "timeout",
  "state": "Idle"
}
```

The agent should loop and call `wait_for_*` again.

---

## `/heartbeat` Tool

Available to both Supervisor and Executor roles.

The caller passes their `agent_id`. The tool:

1. Reads `STATE.json`
2. Validates the caller holds the lease
3. On success: sets `last_heartbeat = now`, `lease_until = now + ttl_secs`, writes state
4. Returns structured JSON

### Success response

```json
{
  "status": "renewed",
  "claimed_by": "executor:codex:1",
  "lease_until": "2026-03-15T10:02:00Z"
}
```

### Error response

```json
{
  "status": "error",
  "code": "<code>",
  "message": "<human-readable description>"
}
```

### Error codes

| Code | Meaning |
|---|---|
| `not_claimed` | No active lease exists |
| `claimed_by_other` | Lease is held by a different agent (message names them) |
| `expired` | Caller's own lease timed out before renewal |
| `invalid_state` | State is not in a leasable state (e.g. Idle, Complete) |

The `/heartbeat` tool does **not** change `TaskState` — it only refreshes the lease timestamps.

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
2. Counts entries whose `args` contain `--role <role>` and `--agent-name <name>`
3. Writes a new entry with `--agent-index <count+1>`

Generated `args` shape:

```json
["serve", "--role", "executor", "--agent-name", "codex", "--agent-index", "1"]
```

---

## `ferrus serve` — New CLI Flags

```sh
ferrus serve --role executor --agent-name codex --agent-index 1
```

`--agent-name` and `--agent-index` are optional; when absent, `claimed_by` defaults to `"<role>:unknown:0"`. This keeps single-agent setups working without any config change.

At startup, the server constructs:

```rust
let agent_id = format!("{}:{}:{}", role, agent_name, agent_index);
```

`agent_id` is passed into each tool handler that needs it (`wait_for_task`, `wait_for_review`, `heartbeat`).

---

## Skill File Updates

- `ferrus-executor/SKILL.md` — document the heartbeat loop: call `/heartbeat` every ~`heartbeat_interval_secs` while working; re-call `wait_for_task` on `"timeout"`
- `ferrus-supervisor/SKILL.md` — same for `wait_for_review` + `/heartbeat`
- `ferrus/SKILL.md` — add `/heartbeat` to the shared tools table; add lease fields to STATE.json reference; add `[lease]` to `ferrus.toml` example

---

## Files Changed

| File | Change |
|---|---|
| `Cargo.toml` | Add `fs2 = "0.4"` dependency |
| `src/config/mod.rs` | Add `LeaseConfig`, `#[serde(default)]` field on `Config` |
| `src/state/machine.rs` | Add 3 fields + 3 helpers to `StateData` |
| `src/state/store.rs` | Add `claim_state(agent_id, ttl)` helper used by wait tools |
| `src/server/tools/wait_for_task.rs` | File-lock claim loop, structured JSON output |
| `src/server/tools/wait_for_review.rs` | File-lock claim loop, structured JSON output |
| `src/server/tools/heartbeat.rs` | New tool (renewal + structured errors) |
| `src/server/tools/mod.rs` | Register `heartbeat` tool |
| `src/server/mod.rs` | Pass `agent_id` into tool context; register heartbeat for both roles |
| `src/cli/commands/serve.rs` | Add `--agent-name`, `--agent-index` flags; construct `agent_id` |
| `src/cli/commands/register.rs` | Count existing entries, write `--agent-name`/`--agent-index` into args |
| `src/cli/commands/init.rs` | Add `[lease]` block to generated `ferrus.toml` template |
| Skill files (3×) | Document heartbeat loop, new tool, new config, new STATE fields |
