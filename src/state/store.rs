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
    if let Ok(state) = read_state().await
        && let Some(path) = state.active_task_path.as_deref()
        && let Ok(contents) = read_path(Path::new(path)).await
    {
        return Ok(contents);
    }
    // Legacy fallback for projects migrated before numbered task artifacts existed.
    read_file("TASK.md").await
}

pub async fn read_task_template() -> Result<String> {
    read_file("TASK.md").await
}

pub async fn read_task_at(task_path: &str) -> Result<String> {
    read_path(Path::new(task_path)).await
}

pub async fn write_task_for_state(state: &StateData, content: &str) -> Result<()> {
    if let Some(path) = state.active_task_path.as_deref() {
        write_path(Path::new(path), content).await?;
    }
    Ok(())
}

pub async fn clear_task_for_state(state: &StateData) -> Result<()> {
    if let Some(path) = state.active_task_path.as_deref() {
        write_path(Path::new(path), "").await?;
    }
    Ok(())
}

pub async fn clear_task_mirror() -> Result<()> {
    Ok(())
}

pub async fn read_review() -> Result<String> {
    if let Ok(state) = read_state().await
        && let Some(path) = active_run_file(&state, "REVIEW.md")
        && let Ok(contents) = read_path(&path).await
    {
        return Ok(contents);
    }
    read_file("REVIEW.md").await
}

pub async fn read_review_for_run_dir(run_dir: &str) -> Result<String> {
    read_path_or_empty(&Path::new(run_dir).join("REVIEW.md")).await
}

pub async fn write_review_for_run_dir(run_dir: &str, content: &str) -> Result<()> {
    write_path(&run_file(run_dir, "REVIEW.md"), content).await
}

pub async fn write_review(content: &str) -> Result<()> {
    if let Ok(state) = read_state().await
        && let Some(path) = active_run_file(&state, "REVIEW.md")
    {
        write_path(&path, content).await?;
    }
    write_file("REVIEW.md", content).await
}

pub async fn write_review_for_state(state: &StateData, content: &str) -> Result<()> {
    if let Some(path) = active_run_file(state, "REVIEW.md") {
        write_path(&path, content).await?;
    }
    write_file("REVIEW.md", content).await
}

pub async fn read_submission() -> Result<String> {
    if let Ok(state) = read_state().await
        && let Some(path) = active_run_file(&state, "SUBMISSION.md")
        && let Ok(contents) = read_path(&path).await
    {
        return Ok(contents);
    }
    read_file("SUBMISSION.md").await
}

pub async fn write_submission(content: &str) -> Result<()> {
    if let Ok(state) = read_state().await
        && let Some(path) = active_run_file(&state, "SUBMISSION.md")
    {
        write_path(&path, content).await?;
    }
    write_file("SUBMISSION.md", content).await
}

pub async fn write_submission_for_state(state: &StateData, content: &str) -> Result<()> {
    if let Some(path) = active_run_file(state, "SUBMISSION.md") {
        write_path(&path, content).await?;
    }
    write_file("SUBMISSION.md", content).await
}

pub async fn read_submission_for_run_dir(run_dir: &str) -> Result<String> {
    read_path_or_empty(&run_file(run_dir, "SUBMISSION.md")).await
}

pub async fn write_submission_for_run_dir(run_dir: &str, content: &str) -> Result<()> {
    write_path(&run_file(run_dir, "SUBMISSION.md"), content).await
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

pub async fn clear_review_for_state(state: &StateData) -> Result<()> {
    write_review_for_state(state, "").await
}

pub async fn clear_review_mirror() -> Result<()> {
    write_file("REVIEW.md", "").await
}

pub async fn clear_submission_for_state(state: &StateData) -> Result<()> {
    write_submission_for_state(state, "").await
}

pub async fn clear_submission_mirror() -> Result<()> {
    write_file("SUBMISSION.md", "").await
}

pub async fn read_question() -> Result<String> {
    if let Ok(state) = read_state().await
        && let Some(path) = active_run_file(&state, "QUESTION.md")
        && let Ok(contents) = read_path(&path).await
    {
        return Ok(contents);
    }
    read_file("QUESTION.md").await
}

pub async fn write_question(content: &str) -> Result<()> {
    if let Ok(state) = read_state().await
        && let Some(path) = active_run_file(&state, "QUESTION.md")
    {
        write_path(&path, content).await?;
    }
    write_file("QUESTION.md", content).await
}

pub async fn write_question_for_run_dir(run_dir: &str, content: &str) -> Result<()> {
    write_path(&run_file(run_dir, "QUESTION.md"), content).await
}

pub async fn read_answer() -> Result<String> {
    if let Ok(state) = read_state().await
        && let Some(path) = active_run_file(&state, "ANSWER.md")
        && let Ok(contents) = read_path(&path).await
    {
        return Ok(contents);
    }
    read_file("ANSWER.md").await
}

pub async fn write_answer(content: &str) -> Result<()> {
    if let Ok(state) = read_state().await
        && let Some(path) = active_run_file(&state, "ANSWER.md")
    {
        write_path(&path, content).await?;
    }
    write_file("ANSWER.md", content).await
}

pub async fn read_consult_request() -> Result<String> {
    if let Ok(state) = read_state().await
        && let Some(path) = active_run_file(&state, "CONSULT_REQUEST.md")
        && let Ok(contents) = read_path(&path).await
    {
        return Ok(contents);
    }
    read_file("CONSULT_REQUEST.md").await
}

pub async fn write_consult_request(content: &str) -> Result<()> {
    if let Ok(state) = read_state().await
        && let Some(path) = active_run_file(&state, "CONSULT_REQUEST.md")
    {
        write_path(&path, content).await?;
    }
    write_file("CONSULT_REQUEST.md", content).await
}

pub async fn write_consult_request_for_run_dir(run_dir: &str, content: &str) -> Result<()> {
    write_path(&run_file(run_dir, "CONSULT_REQUEST.md"), content).await
}

pub async fn read_consult_request_for_run_dir(run_dir: &str) -> Result<String> {
    read_path(&run_file(run_dir, "CONSULT_REQUEST.md")).await
}

pub async fn clear_consult_request_for_run_dir(run_dir: &str) -> Result<()> {
    write_path(&run_file(run_dir, "CONSULT_REQUEST.md"), "").await
}

pub async fn clear_consult_request() -> Result<()> {
    write_consult_request("").await
}

pub async fn clear_consult_request_mirror() -> Result<()> {
    write_file("CONSULT_REQUEST.md", "").await
}

pub async fn read_consult_response() -> Result<String> {
    if let Ok(state) = read_state().await
        && let Some(path) = active_run_file(&state, "CONSULT_RESPONSE.md")
        && let Ok(contents) = read_path(&path).await
    {
        return Ok(contents);
    }
    read_file("CONSULT_RESPONSE.md").await
}

#[allow(dead_code)]
pub async fn write_consult_response(content: &str) -> Result<()> {
    if let Ok(state) = read_state().await
        && let Some(path) = active_run_file(&state, "CONSULT_RESPONSE.md")
    {
        write_path(&path, content).await?;
    }
    write_file("CONSULT_RESPONSE.md", content).await
}

pub async fn clear_consult_response() -> Result<()> {
    write_consult_response("").await
}

pub async fn read_consult_response_for_run_dir(run_dir: &str) -> Result<String> {
    read_path(&run_file(run_dir, "CONSULT_RESPONSE.md")).await
}

pub async fn write_consult_response_for_run_dir(run_dir: &str, content: &str) -> Result<()> {
    write_path(&run_file(run_dir, "CONSULT_RESPONSE.md"), content).await
}

pub async fn clear_consult_response_for_run_dir(run_dir: &str) -> Result<()> {
    write_path(&run_file(run_dir, "CONSULT_RESPONSE.md"), "").await
}

pub async fn clear_consult_response_mirror() -> Result<()> {
    write_file("CONSULT_RESPONSE.md", "").await
}

pub async fn read_last_spec_path() -> Result<String> {
    read_file("LAST_SPEC_PATH").await
}

pub async fn write_last_spec_path(content: &str) -> Result<()> {
    write_file("LAST_SPEC_PATH", content).await
}

pub async fn clear_last_spec_path() -> Result<()> {
    write_last_spec_path("").await
}

pub async fn clear_question() -> Result<()> {
    write_question("").await
}

pub async fn clear_question_mirror() -> Result<()> {
    write_file("QUESTION.md", "").await
}

pub async fn clear_answer() -> Result<()> {
    write_answer("").await
}

pub async fn clear_answer_mirror() -> Result<()> {
    write_file("ANSWER.md", "").await
}

async fn read_file(filename: &str) -> Result<String> {
    let p = path(filename);
    read_path(&p).await
}

async fn write_file(filename: &str, content: &str) -> Result<()> {
    let p = path(filename);
    write_path(&p, content).await
}

async fn read_path(path: &Path) -> Result<String> {
    tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("Failed to read {}", path.display()))
}

async fn read_path_or_empty(path: &Path) -> Result<String> {
    match tokio::fs::read_to_string(path).await {
        Ok(contents) => Ok(contents),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(err).with_context(|| format!("Failed to read {}", path.display())),
    }
}

async fn write_path(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    tokio::fs::write(path, content)
        .await
        .with_context(|| format!("Failed to write {}", path.display()))
}

fn active_run_file(state: &StateData, filename: &str) -> Option<PathBuf> {
    state
        .active_run_dir
        .as_deref()
        .map(|dir| Path::new(dir).join(filename))
}

fn run_file(run_dir: &str, filename: &str) -> PathBuf {
    Path::new(run_dir).join(filename)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn setup() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ferrus")).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        (dir, previous)
    }

    fn teardown(previous: PathBuf) {
        std::env::set_current_dir(previous).unwrap();
    }

    #[tokio::test]
    async fn active_task_artifacts_are_written_without_rewriting_task_template() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        write_file("TASK.md", "task template").await.unwrap();
        let mut state = StateData::default();
        state.set_active_task_artifacts(
            "t-001".to_string(),
            ".ferrus/tasks/t-001.md".to_string(),
            ".ferrus/runs/t-001".to_string(),
        );
        write_state(&state).await.unwrap();

        write_task_for_state(&state, "task body").await.unwrap();
        write_review("review body").await.unwrap();
        write_submission("submission body").await.unwrap();
        write_question("question body").await.unwrap();
        write_answer("answer body").await.unwrap();
        write_consult_request("consult request body").await.unwrap();
        write_consult_response("consult response body")
            .await
            .unwrap();

        assert_eq!(read_task().await.unwrap(), "task body");
        assert_eq!(read_review().await.unwrap(), "review body");
        assert_eq!(read_submission().await.unwrap(), "submission body");
        assert_eq!(read_question().await.unwrap(), "question body");
        assert_eq!(read_answer().await.unwrap(), "answer body");
        assert_eq!(
            read_consult_request().await.unwrap(),
            "consult request body"
        );
        assert_eq!(
            read_consult_response().await.unwrap(),
            "consult response body"
        );
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/TASK.md").await.unwrap(),
            "task template"
        );
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/tasks/t-001.md")
                .await
                .unwrap(),
            "task body"
        );
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/runs/t-001/REVIEW.md")
                .await
                .unwrap(),
            "review body"
        );
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/runs/t-001/SUBMISSION.md")
                .await
                .unwrap(),
            "submission body"
        );
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/runs/t-001/QUESTION.md")
                .await
                .unwrap(),
            "question body"
        );
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/runs/t-001/ANSWER.md")
                .await
                .unwrap(),
            "answer body"
        );
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/runs/t-001/CONSULT_REQUEST.md")
                .await
                .unwrap(),
            "consult request body"
        );
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/runs/t-001/CONSULT_RESPONSE.md")
                .await
                .unwrap(),
            "consult response body"
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn scoped_artifact_reads_do_not_depend_on_active_state() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        let mut state = StateData::default();
        state.set_active_task_artifacts(
            "t-001".to_string(),
            ".ferrus/tasks/t-001.md".to_string(),
            ".ferrus/runs/t-001".to_string(),
        );
        write_state(&state).await.unwrap();

        write_path(Path::new(".ferrus/tasks/t-002.md"), "second task")
            .await
            .unwrap();
        write_path(Path::new(".ferrus/runs/t-002/REVIEW.md"), "second review")
            .await
            .unwrap();

        assert_eq!(
            read_task_at(".ferrus/tasks/t-002.md").await.unwrap(),
            "second task"
        );
        assert_eq!(
            read_review_for_run_dir(".ferrus/runs/t-002").await.unwrap(),
            "second review"
        );
        assert_eq!(
            read_review_for_run_dir(".ferrus/runs/t-missing")
                .await
                .unwrap(),
            ""
        );

        teardown(previous);
    }
}
