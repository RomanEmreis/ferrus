pub const ROLE_SUPERVISOR: &str = "supervisor";
pub const ROLE_EXECUTOR: &str = "executor";
pub const DEFAULT_AGENT_INDEX: u32 = 1;

pub fn agent_id(role: &str, vendor: &str, index: u32) -> String {
    format!("{role}:{vendor}:{index}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_structured_agent_id() {
        assert_eq!(agent_id("executor", "codex", 1), "executor:codex:1");
    }
}
