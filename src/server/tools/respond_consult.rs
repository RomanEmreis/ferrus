use anyhow::Result;
use neva::prelude::*;
use tracing::info;

use crate::{
    project::{self, RuntimeTaskContext},
    state::{
        machine::{StateData, TaskState},
        store,
    },
};

use super::{runtime_task_context_for_agent_best_effort, tool_err};

pub const DESCRIPTION: &str = "Record the Supervisor's consultation response. \
     Writes the response to CONSULT_RESPONSE.md. Must only be called while state is Consultation.";

pub const INPUT_SCHEMA: &str = r#"{
    "properties": {
        "response": {
            "type": "string",
            "description": "The Supervisor's consultation response"
        }
    },
    "required": ["response"]
}"#;

pub async fn handler_for_agent(agent_id: &str, response: String) -> Result<String, Error> {
    run(Some(agent_id), response).await.map_err(tool_err)
}

async fn run(agent_id: Option<&str>, response: String) -> Result<String> {
    if response.trim().is_empty() {
        anyhow::bail!("Consultation response cannot be empty.");
    }

    let state = store::read_state().await.ok();
    let runtime_context = match agent_id {
        Some(agent_id) => runtime_task_context_for_agent_best_effort(agent_id)
            .await
            .or(
                match project::attach_running_run_to_next_consultation(agent_id).await {
                    Ok(context) => context,
                    Err(err) => {
                        tracing::warn!(
                            error = ?err,
                            agent_id,
                            "failed to attach supervisor run to consultation"
                        );
                        None
                    }
                },
            ),
        None => None,
    };

    let context_is_consultation = matches!(
        runtime_context
            .as_ref()
            .map(|context| context.status.as_str()),
        Some("consultation")
    );
    let state_is_consultation = state
        .as_ref()
        .is_some_and(|state| state.state == TaskState::Consultation);
    if !context_is_consultation && !state_is_consultation {
        let current_state = state
            .as_ref()
            .map(|state| format!("{:?}", state.state))
            .unwrap_or_else(|| "unavailable".to_string());
        anyhow::bail!(
            "Cannot respond to consultation from state {current_state}. /respond_consult is only valid in Consultation state.",
        );
    }

    write_consult_response(state.as_ref(), runtime_context.as_ref(), &response).await?;
    info!("Consultation response recorded");
    Ok("Consultation response recorded in `.ferrus/CONSULT_RESPONSE.md`.".to_string())
}

async fn write_consult_response(
    state: Option<&StateData>,
    context: Option<&RuntimeTaskContext>,
    response: &str,
) -> Result<()> {
    if let Some(context) = context {
        store::write_consult_response_for_run_dir(&context.run_dir, response).await?;
        if let Some(state) = state
            && state.active_task_id.as_deref() == Some(context.task_id.as_str())
        {
            store::write_consult_response(response).await?;
        }
        return Ok(());
    }
    store::write_consult_response(response).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::machine::StateData;
    use tempfile::TempDir;

    async fn setup() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ferrus/tasks")).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
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
        (dir, previous)
    }

    fn teardown(previous: std::path::PathBuf) {
        std::env::set_current_dir(previous).unwrap();
    }

    #[tokio::test]
    async fn respond_consult_writes_scoped_response_for_attached_consultation_run() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        let mut state = StateData {
            state: TaskState::Executing,
            ..Default::default()
        };
        state.set_active_task_artifacts(
            "t-001".to_string(),
            ".ferrus/tasks/t-001.md".to_string(),
            ".ferrus/runs/t-001".to_string(),
        );
        store::write_state(&state).await.unwrap();
        crate::project::record_task_status("t-007", ".ferrus/tasks/t-007.md", "executing")
            .await
            .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "executor:codex:7", 60)
            .await
            .unwrap();
        crate::project::record_task_consultation_requested("t-007", "executing")
            .await
            .unwrap();
        crate::project::record_run_started("supervisor", "supervisor:codex:1", std::process::id())
            .await
            .unwrap();
        crate::project::attach_running_run_to_next_consultation("supervisor:codex:1")
            .await
            .unwrap();

        run(Some("supervisor:codex:1"), "Use option A.".to_string())
            .await
            .unwrap();

        assert_eq!(
            store::read_consult_response_for_run_dir(".ferrus/runs/t-007")
                .await
                .unwrap(),
            "Use option A."
        );
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/CONSULT_RESPONSE.md")
                .await
                .unwrap_or_default(),
            ""
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn respond_consult_uses_database_context_when_state_json_is_absent() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status("t-007", ".ferrus/tasks/t-007.md", "executing")
            .await
            .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "executor:codex:7", 60)
            .await
            .unwrap();
        crate::project::record_task_consultation_requested("t-007", "executing")
            .await
            .unwrap();
        crate::project::record_run_started("supervisor", "supervisor:codex:1", std::process::id())
            .await
            .unwrap();
        crate::project::attach_running_run_to_next_consultation("supervisor:codex:1")
            .await
            .unwrap();

        run(Some("supervisor:codex:1"), "Use option A.".to_string())
            .await
            .unwrap();

        assert!(store::read_state().await.is_err());
        assert_eq!(
            store::read_consult_response_for_run_dir(".ferrus/runs/t-007")
                .await
                .unwrap(),
            "Use option A."
        );

        teardown(previous);
    }
}
