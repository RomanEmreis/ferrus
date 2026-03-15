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

const fn default_max_check_retries() -> u32 {
    5
}
const fn default_max_review_cycles() -> u32 {
    3
}
const fn default_max_feedback_lines() -> usize {
    30
}
const fn default_wait_timeout_secs() -> u64 {
    3600
}
const fn default_ttl_secs() -> u64 {
    90
}
const fn default_heartbeat_interval_secs() -> u64 {
    30
}

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

    #[test]
    fn limits_config_defaults_without_values() {
        let toml = r#"
[checks]
commands = ["cargo test"]

[limits]
"#;
        let config: Config = toml::from_str(toml).unwrap();

        assert_eq!(config.limits.max_check_retries, 5);
        assert_eq!(config.limits.max_review_cycles, 3);
        assert_eq!(config.limits.max_feedback_lines, 30);
        assert_eq!(config.limits.wait_timeout_secs, 3600);
    }

    #[test]
    fn config_round_trips_explicit_values() {
        let toml = r#"
[checks]
commands = ["cargo test", "cargo clippy -- -D warnings"]

[limits]
max_check_retries = 7
max_review_cycles = 4
max_feedback_lines = 12
wait_timeout_secs = 900

[lease]
ttl_secs = 120
heartbeat_interval_secs = 45
"#;
        let config: Config = toml::from_str(toml).unwrap();

        assert_eq!(
            config.checks.commands,
            vec![
                "cargo test".to_string(),
                "cargo clippy -- -D warnings".to_string()
            ]
        );
        assert_eq!(config.limits.max_check_retries, 7);
        assert_eq!(config.limits.max_review_cycles, 4);
        assert_eq!(config.limits.max_feedback_lines, 12);
        assert_eq!(config.limits.wait_timeout_secs, 900);
        assert_eq!(config.lease.ttl_secs, 120);
        assert_eq!(config.lease.heartbeat_interval_secs, 45);
    }
}
