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
