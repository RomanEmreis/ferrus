use neva::prelude::*;

use crate::{project, state::store, templates::SPEC_TEMPLATE};

fn to_err(e: impl std::fmt::Display) -> Error {
    Error::new(
        ErrorCode::InternalError,
        std::io::Error::other(e.to_string()),
    )
}

pub async fn read_for_agent(
    agent_id: Option<&str>,
    file: String,
) -> Result<ReadResourceResult, Error> {
    let (mime, content) = match file.as_str() {
        "task" => (
            "text/markdown",
            read_current_task_for_agent(agent_id)
                .await
                .map_err(to_err)?,
        ),
        "task_template" => (
            "text/markdown",
            store::read_task_template().await.map_err(to_err)?,
        ),
        "review" => ("text/markdown", store::read_review().await.map_err(to_err)?),
        "submission" => (
            "text/markdown",
            store::read_submission().await.map_err(to_err)?,
        ),
        "question" => (
            "text/markdown",
            tokio::fs::read_to_string(store::resolve_project_path(".ferrus/QUESTION.md"))
                .await
                .unwrap_or_default(),
        ),
        "answer" => (
            "text/markdown",
            tokio::fs::read_to_string(store::resolve_project_path(".ferrus/ANSWER.md"))
                .await
                .unwrap_or_default(),
        ),
        "consult_template" => (
            "text/markdown",
            tokio::fs::read_to_string(store::resolve_project_path(".ferrus/CONSULT_TEMPLATE.md"))
                .await
                .unwrap_or_default(),
        ),
        "spec_template" => (
            "text/markdown",
            tokio::fs::read_to_string(store::resolve_project_path(".ferrus/SPEC_TEMPLATE.md"))
                .await
                .unwrap_or_else(|_| SPEC_TEMPLATE.to_string()),
        ),
        "consult_request" => (
            "text/markdown",
            read_consult_request_for_agent(agent_id)
                .await
                .map_err(to_err)?,
        ),
        "consult_response" => (
            "text/markdown",
            read_consult_response_for_agent(agent_id)
                .await
                .map_err(to_err)?,
        ),
        "state" => {
            let state = store::read_state().await.map_err(to_err)?;
            let json = serde_json::to_string_pretty(&state).map_err(to_err)?;
            ("application/json", json)
        }
        _ => {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                std::io::Error::other(format!("Unknown ferrus resource: {file}")),
            ));
        }
    };

    let uri = format!("ferrus://{file}");
    Ok(ReadResourceResult::from(
        TextResourceContents::new(uri, content).with_mime(mime),
    ))
}

async fn read_consult_request_for_agent(agent_id: Option<&str>) -> anyhow::Result<String> {
    if let Some(agent_id) = agent_id
        && let Some(context) = project::runtime_task_context_for_agent(agent_id).await?
        && let Ok(contents) = store::read_consult_request_for_run_dir(&context.run_dir).await
    {
        return Ok(contents);
    }
    store::read_consult_request().await
}

async fn read_consult_response_for_agent(agent_id: Option<&str>) -> anyhow::Result<String> {
    if let Some(agent_id) = agent_id
        && let Some(context) = project::runtime_task_context_for_agent(agent_id).await?
        && let Ok(contents) = store::read_consult_response_for_run_dir(&context.run_dir).await
    {
        return Ok(contents);
    }
    store::read_consult_response().await
}

async fn read_current_task_for_agent(agent_id: Option<&str>) -> anyhow::Result<String> {
    if let Some(agent_id) = agent_id
        && let Some(context) = project::runtime_task_context_for_agent(agent_id).await?
        && let Ok(contents) = store::read_task_at(&context.task_path).await
    {
        return Ok(contents);
    }
    store::read_task().await
}

/// Handler for `ferrus://task/{task_id}` resource reads.
pub async fn read_task_by_id(task_id: String) -> Result<ReadResourceResult, Error> {
    if !valid_task_id(&task_id) {
        return Err(Error::new(
            ErrorCode::InvalidRequest,
            std::io::Error::other(format!("Invalid ferrus task id: {task_id}")),
        ));
    }

    let uri = format!("ferrus://task/{task_id}");
    let path = format!(".ferrus/tasks/{task_id}.md");
    let content = store::read_task_at(&path).await.map_err(to_err)?;
    Ok(ReadResourceResult::from(
        TextResourceContents::new(uri, content).with_mime("text/markdown"),
    ))
}

fn valid_task_id(task_id: &str) -> bool {
    let Some(number) = task_id.strip_prefix("t-") else {
        return false;
    };
    !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use neva::types::ResourceContents;
    use tempfile::TempDir;

    async fn setup() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ferrus/tasks")).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        (dir, previous)
    }

    fn teardown(previous: std::path::PathBuf) {
        std::env::set_current_dir(previous).unwrap();
    }

    fn text(result: ReadResourceResult) -> String {
        match result.contents.into_iter().next().unwrap() {
            ResourceContents::Text(text) => text.text,
            _ => panic!("expected text resource"),
        }
    }

    #[tokio::test]
    async fn task_template_reads_template_file() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        tokio::fs::write(".ferrus/TASK.md", "template body")
            .await
            .unwrap();

        let result = read_for_agent(None, "task_template".to_string())
            .await
            .unwrap();

        assert_eq!(text(result), "template body");
        teardown(previous);
    }

    #[tokio::test]
    async fn task_id_resource_reads_numbered_task_artifact() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        tokio::fs::write(".ferrus/tasks/t-042.md", "specific task")
            .await
            .unwrap();

        let result = read_task_by_id("t-042".to_string()).await.unwrap();

        assert_eq!(text(result), "specific task");
        teardown(previous);
    }

    #[tokio::test]
    async fn task_resource_prefers_agent_runtime_context() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (dir, previous) = setup().await;
        let data_dir = dir.path().join(".ferrus/projects/test-project");
        tokio::fs::create_dir_all(&data_dir).await.unwrap();
        let local_ref = crate::project::LocalProjectRef {
            project_id: "test-project".to_string(),
            name: "test".to_string(),
            data_dir: data_dir.to_string_lossy().into_owned(),
        };
        tokio::fs::write(
            ".ferrus/project.toml",
            toml::to_string_pretty(&local_ref).unwrap(),
        )
        .await
        .unwrap();
        tokio::fs::write(".ferrus/TASK.md", "template body")
            .await
            .unwrap();
        tokio::fs::write(".ferrus/tasks/t-007.md", "assigned task")
            .await
            .unwrap();
        crate::project::record_task_status("t-007", ".ferrus/tasks/t-007.md", "executing")
            .await
            .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "executor:codex:7", 60)
            .await
            .unwrap();

        let result = read_for_agent(Some("executor:codex:7"), "task".to_string())
            .await
            .unwrap();

        assert_eq!(text(result), "assigned task");
        teardown(previous);
    }

    #[tokio::test]
    async fn task_id_resource_rejects_path_traversal() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;

        let err = read_task_by_id("../TASK".to_string()).await.unwrap_err();

        assert!(err.to_string().contains("Invalid ferrus task id"));
        teardown(previous);
    }
}
