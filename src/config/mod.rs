use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub checks: ChecksConfig,
    pub limits: LimitsConfig,
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

const fn default_max_check_retries() -> u32 { 5 }
const fn default_max_review_cycles() -> u32 { 3 }
const fn default_max_feedback_lines() -> usize { 30 }
const fn default_wait_timeout_secs() -> u64 { 3600 }

impl Config {
    pub async fn load() -> Result<Self> {
        let contents = tokio::fs::read_to_string("ferrus.toml")
            .await
            .context("ferrus.toml not found — run `ferrus init` first")?;
        toml::from_str(&contents).context("Failed to parse ferrus.toml")
    }
}
