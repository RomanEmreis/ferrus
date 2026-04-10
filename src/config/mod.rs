use anyhow::{Context, Result};
use serde::Deserialize;
use std::sync::Arc;

use crate::agents::{parse_executor_agent, parse_supervisor_agent, ExecutorAgent, SupervisorAgent};

#[derive(Debug)]
#[allow(dead_code)]
pub struct Config {
    pub checks: ChecksConfig,
    pub limits: LimitsConfig,
    pub lease: LeaseConfig,
    pub hq: Option<HqConfig>,
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

pub struct HqConfig {
    pub supervisor: Arc<dyn SupervisorAgent>,
    pub executor: Arc<dyn ExecutorAgent>,
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    checks: ChecksConfig,
    limits: LimitsConfig,
    #[serde(default)]
    lease: LeaseConfig,
    #[serde(default)]
    hq: Option<RawHqConfig>,
}

#[derive(Debug, Deserialize)]
struct RawHqConfig {
    supervisor: String,
    executor: String,
}

impl std::fmt::Debug for HqConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HqConfig")
            .field("supervisor", &self.supervisor.name())
            .field("executor", &self.executor.name())
            .finish()
    }
}

impl HqConfig {
    pub fn supervisor_name(&self) -> &'static str {
        self.supervisor.name()
    }

    pub fn executor_name(&self) -> &'static str {
        self.executor.name()
    }
}

impl TryFrom<RawConfig> for Config {
    type Error = anyhow::Error;

    fn try_from(raw: RawConfig) -> Result<Self> {
        let hq = raw.hq.map(HqConfig::try_from).transpose()?;
        Ok(Self {
            checks: raw.checks,
            limits: raw.limits,
            lease: raw.lease,
            hq,
        })
    }
}

impl TryFrom<RawHqConfig> for HqConfig {
    type Error = anyhow::Error;

    fn try_from(raw: RawHqConfig) -> Result<Self> {
        Ok(Self {
            supervisor: parse_supervisor_agent(&raw.supervisor)?,
            executor: parse_executor_agent(&raw.executor)?,
        })
    }
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
        Self::from_toml(&contents)
    }

    fn from_toml(contents: &str) -> Result<Self> {
        let raw: RawConfig = toml::from_str(contents).context("Failed to parse ferrus.toml")?;
        raw.try_into()
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
        let config = Config::from_toml(toml).unwrap();
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
        let config = Config::from_toml(toml).unwrap();

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
        let config = Config::from_toml(toml).unwrap();

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

    #[test]
    fn hq_config_absent_gives_none() {
        let toml = "[checks]\ncommands = [\"cargo test\"]\n[limits]\n";
        let config = Config::from_toml(toml).unwrap();
        assert!(config.hq.is_none());
    }

    #[test]
    fn hq_config_parses_when_present() {
        let toml = "[checks]\ncommands = [\"cargo test\"]\n[limits]\n[hq]\nsupervisor = \"claude-code\"\nexecutor = \"codex\"\n";
        let config = Config::from_toml(toml).unwrap();
        let hq = config.hq.unwrap();
        assert_eq!(hq.supervisor_name(), "claude-code");
        assert_eq!(hq.executor_name(), "codex");
    }

    #[test]
    fn invalid_hq_agent_is_rejected_at_load_time() {
        let toml = "[checks]\ncommands = [\"cargo test\"]\n[limits]\n[hq]\nsupervisor = \"unknown\"\nexecutor = \"codex\"\n";
        let err = Config::from_toml(toml).unwrap_err().to_string();
        assert!(err.contains("Unknown supervisor agent 'unknown'"));
    }
}
