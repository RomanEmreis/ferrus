use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use super::machine::StateData;

const FERRUS_DIR: &str = ".ferrus";
const LOGS_DIR: &str = ".ferrus/logs";

fn path(filename: &str) -> PathBuf {
    Path::new(FERRUS_DIR).join(filename)
}

pub async fn read_state() -> Result<StateData> {
    let p = path("STATE.json");
    let contents = tokio::fs::read_to_string(&p)
        .await
        .with_context(|| format!("Cannot read {} — run `ferrus init` first", p.display()))?;
    serde_json::from_str(&contents).context("Failed to parse STATE.json")
}

pub async fn write_state(state: &StateData) -> Result<()> {
    let p = path("STATE.json");
    let json = serde_json::to_string_pretty(state).context("Failed to serialize state")?;
    tokio::fs::write(&p, json)
        .await
        .with_context(|| format!("Failed to write {}", p.display()))
}

pub async fn read_task() -> Result<String> {
    read_file("TASK.md").await
}

pub async fn write_task(content: &str) -> Result<()> {
    write_file("TASK.md", content).await
}

pub async fn read_feedback() -> Result<String> {
    read_file("FEEDBACK.md").await
}

pub async fn write_feedback(content: &str) -> Result<()> {
    write_file("FEEDBACK.md", content).await
}

pub async fn read_review() -> Result<String> {
    read_file("REVIEW.md").await
}

pub async fn write_review(content: &str) -> Result<()> {
    write_file("REVIEW.md", content).await
}

pub async fn clear_feedback() -> Result<()> {
    write_feedback("").await
}

/// Write a full check log to `.ferrus/logs/check_{attempt}_{ts}.txt`.
/// Creates the logs directory if it doesn't exist. Returns the file path.
pub async fn write_check_log(attempt: u32, ts: u64, content: &str) -> Result<PathBuf> {
    let logs_dir = Path::new(LOGS_DIR);
    tokio::fs::create_dir_all(logs_dir)
        .await
        .with_context(|| format!("Failed to create {}", logs_dir.display()))?;
    let filename = format!("check_{attempt}_{ts}.txt");
    let p = logs_dir.join(&filename);
    tokio::fs::write(&p, content)
        .await
        .with_context(|| format!("Failed to write {}", p.display()))?;
    Ok(p)
}

pub async fn clear_review() -> Result<()> {
    write_review("").await
}

async fn read_file(filename: &str) -> Result<String> {
    let p = path(filename);
    tokio::fs::read_to_string(&p)
        .await
        .with_context(|| format!("Failed to read {}", p.display()))
}

async fn write_file(filename: &str, content: &str) -> Result<()> {
    let p = path(filename);
    tokio::fs::write(&p, content)
        .await
        .with_context(|| format!("Failed to write {}", p.display()))
}
