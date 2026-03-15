use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
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
    /// The full output is always written to .ferrus/logs/.
    #[serde(default = "default_max_feedback_lines")]
    pub max_feedback_lines: usize,
    /// How long (in seconds) /wait_for_task and /wait_for_review will poll before timing out.
    #[serde(default = "default_wait_timeout_secs")]
    pub wait_timeout_secs: u64,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_config_defaults_without_block() {
        let toml = r#"
[checks]
commands = ["cargo test"]

[limits]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.lease.ttl_secs, 90);
        assert_eq!(config.lease.heartbeat_interval_secs, 30);
    }
}
