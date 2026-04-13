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
    TaskState::Fixing,
    TaskState::Addressing,
    TaskState::Checking,
    TaskState::Consultation,
    TaskState::Reviewing,
];

pub async fn handler(agent_id: String) -> Result<String, Error> {
    run(&agent_id).await.map_err(tool_err)
}

async fn run(agent_id: &str) -> Result<String> {
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
        })
        .to_string());
    }

    // Step 3: state must be leasable.
    if !LEASABLE_STATES.contains(&state.state) {
        drop(lock_file);
        return Ok(json!({
            "status": "error",
            "code": "invalid_state",
            "message": format!("State {:?} cannot hold a lease", state.state)
        })
        .to_string());
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
    })
    .to_string())
}
