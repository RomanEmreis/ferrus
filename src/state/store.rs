use anyhow::{Context, Result};
use std::fs::File;
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
    let stamped = StateData {
        updated_at: chrono::Utc::now(),
        owner_pid: std::process::id(),
        ..state.clone()
    };
    let json = serde_json::to_string_pretty(&stamped).context("Failed to serialize state")?;
    let tmp = path("STATE.json.tmp");
    let dest = path("STATE.json");
    tokio::fs::write(&tmp, &json)
        .await
        .with_context(|| format!("Failed to write {}", tmp.display()))?;
    tokio::fs::rename(&tmp, &dest)
        .await
        .with_context(|| format!("Failed to rename {} → {}", tmp.display(), dest.display()))
}

/// Open `.ferrus/STATE.lock` for use with `fs2::FileExt::lock_exclusive`.
/// The file must exist (created by `ferrus init`). Returns an open `std::fs::File`.
#[allow(dead_code)]
pub fn open_lock_file() -> Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .open(path("STATE.lock"))
        .with_context(|| "Cannot open .ferrus/STATE.lock — run `ferrus init` first")
}

/// Set the three lease fields on `state` and persist to disk.
/// Callers are responsible for holding the STATE.lock exclusive lock before
/// calling this function. Does not acquire the lock itself.
#[allow(dead_code)]
pub async fn claim_state(agent_id: &str, ttl_secs: u64, state: &mut StateData) -> Result<()> {
    let now = chrono::Utc::now();
    state.claimed_by = Some(agent_id.to_string());
    // chrono::Duration::try_seconds returns None only for values exceeding ~292 billion years;
    // the unwrap_or fallback to Duration::MAX is unreachable under any realistic TTL config.
    state.lease_until =
        Some(now + chrono::Duration::try_seconds(ttl_secs as i64).unwrap_or(chrono::Duration::MAX));
    state.last_heartbeat = Some(now);
    write_state(state).await
}

pub async fn read_task() -> Result<String> {
    read_file("TASK.md").await
}

pub async fn write_task(content: &str) -> Result<()> {
    write_file("TASK.md", content).await
}

pub async fn clear_task() -> Result<()> {
    write_task("").await
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

pub async fn read_submission() -> Result<String> {
    read_file("SUBMISSION.md").await
}

pub async fn write_submission(content: &str) -> Result<()> {
    write_file("SUBMISSION.md", content).await
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

pub async fn clear_submission() -> Result<()> {
    write_submission("").await
}

pub async fn read_question() -> Result<String> {
    read_file("QUESTION.md").await
}

pub async fn write_question(content: &str) -> Result<()> {
    write_file("QUESTION.md", content).await
}

pub async fn read_answer() -> Result<String> {
    read_file("ANSWER.md").await
}

pub async fn write_answer(content: &str) -> Result<()> {
    write_file("ANSWER.md", content).await
}

pub async fn read_consult_request() -> Result<String> {
    read_file("CONSULT_REQUEST.md").await
}

pub async fn write_consult_request(content: &str) -> Result<()> {
    write_file("CONSULT_REQUEST.md", content).await
}

pub async fn clear_consult_request() -> Result<()> {
    write_file("CONSULT_REQUEST.md", "").await
}

pub async fn read_consult_response() -> Result<String> {
    read_file("CONSULT_RESPONSE.md").await
}

#[allow(dead_code)]
pub async fn write_consult_response(content: &str) -> Result<()> {
    write_file("CONSULT_RESPONSE.md", content).await
}

pub async fn clear_consult_response() -> Result<()> {
    write_file("CONSULT_RESPONSE.md", "").await
}

pub async fn clear_question() -> Result<()> {
    write_file("QUESTION.md", "").await
}

pub async fn clear_answer() -> Result<()> {
    write_file("ANSWER.md", "").await
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
