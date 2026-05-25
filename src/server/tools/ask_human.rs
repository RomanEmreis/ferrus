use anyhow::Result;
use tracing::info;

use crate::{
    config::Config,
    project::{self, RuntimeTaskContext},
    state::{
        machine::{StateData, TaskState},
        store,
    },
};

use super::{ensure_lease_owner_or_reclaim, runtime_task_context_for_agent_best_effort, tool_err};

pub const DESCRIPTION: &str = "Ask the human a question. \
     Writes the question to QUESTION.md, transitions state to AwaitingHuman, \
     and returns immediately. You MUST call /wait_for_answer immediately after \
     to block until the human responds — do not call any other tools in between. \
     Can be called from Executing, Addressing, Consultation, or Reviewing state.";

pub const INPUT_SCHEMA: &str = r#"{
    "properties": {
        "question": {
            "type": "string",
            "description": "The question to ask the human"
        }
    },
    "required": ["question"]
}"#;

pub async fn handler_for_agent(
    agent_id: &str,
    question: String,
) -> Result<String, neva::prelude::Error> {
    run(agent_id, question).await.map_err(tool_err)
}

async fn run(agent_id: &str, question: String) -> Result<String> {
    let config = Config::load().await?;
    let runtime_context = runtime_task_context_for_agent_best_effort(agent_id).await;
    let mut state = store::read_state().await.ok();
    ensure_can_ask_human(
        &mut state,
        runtime_context.as_ref(),
        agent_id,
        config.lease.ttl_secs,
    )
    .await?;

    write_question(state.as_ref(), runtime_context.as_ref(), &question).await?;
    clear_answer(state.as_ref(), runtime_context.as_ref()).await?;

    let use_legacy_state = should_use_legacy_state(state.as_ref(), runtime_context.as_ref());
    let paused = if use_legacy_state {
        let state = state.as_mut().ok_or_else(|| {
            anyhow::anyhow!("Cannot ask human for legacy state: STATE.json is missing")
        })?;
        let paused = state.ask_human()?;
        state.awaiting_human_by = Some(agent_id.to_string());
        store::write_state(state).await?;
        if let Some(context) = runtime_context.as_ref() {
            project::record_task_human_question_requested(
                &context.task_id,
                project::task_status_for_state(&paused),
                agent_id,
            )
            .await?;
        }
        format!("{paused:?}")
    } else if let Some(context) = runtime_context.as_ref() {
        project::record_task_human_question_requested(&context.task_id, &context.status, agent_id)
            .await?;
        context.status.clone()
    } else {
        anyhow::bail!("Cannot ask human without runtime task context");
    };

    info!(paused, "Task → AwaitingHuman");
    Ok(format!(
        "Your question has been written to `.ferrus/QUESTION.md`.\n\
         State is now AwaitingHuman (paused from {paused}).\n\
         Call /wait_for_answer immediately to block until the human responds.\n\
         Do NOT call any other tools while waiting."
    ))
}

async fn ensure_can_ask_human(
    state: &mut Option<StateData>,
    context: Option<&RuntimeTaskContext>,
    agent_id: &str,
    ttl_secs: u64,
) -> Result<()> {
    if !can_ask_from_state_or_context(state.as_ref(), context) {
        let current_state = state
            .as_ref()
            .map(|state| format!("{:?}", state.state))
            .unwrap_or_else(|| "unavailable".to_string());
        anyhow::bail!(
            "Cannot ask human from state {current_state}. /ask_human is only available while active work is in progress.",
        );
    }
    if can_supervisor_ask_during_consultation(state.as_ref(), context, agent_id) {
        return Ok(());
    }
    let state = state.get_or_insert_with(StateData::default);
    ensure_lease_owner_or_reclaim(state, agent_id, ttl_secs).await
}

fn can_ask_from_state_or_context(
    state: Option<&StateData>,
    context: Option<&RuntimeTaskContext>,
) -> bool {
    state.is_some_and(|state| {
        matches!(
            state.state,
            TaskState::Executing
                | TaskState::Addressing
                | TaskState::Consultation
                | TaskState::Reviewing
        )
    }) || matches!(
        context.map(|context| context.status.as_str()),
        Some("executing" | "addressing" | "consultation" | "reviewing")
    )
}

fn can_supervisor_ask_during_consultation(
    state: Option<&StateData>,
    context: Option<&RuntimeTaskContext>,
    agent_id: &str,
) -> bool {
    if !is_supervisor(agent_id) {
        return false;
    }
    if should_use_legacy_state(state, context) {
        return state.is_some_and(|state| state.state == TaskState::Consultation);
    }
    matches!(
        context.map(|context| context.status.as_str()),
        Some("consultation")
    )
}

async fn write_question(
    state: Option<&StateData>,
    context: Option<&RuntimeTaskContext>,
    question: &str,
) -> Result<()> {
    if let Some(context) = context {
        store::write_question_for_run_dir(&context.run_dir, question).await?;
        if let Some(state) = state
            && state.active_task_id.as_deref() == Some(context.task_id.as_str())
        {
            store::write_question(question).await?;
        }
        return Ok(());
    }
    store::write_question(question).await
}

async fn clear_answer(
    state: Option<&StateData>,
    context: Option<&RuntimeTaskContext>,
) -> Result<()> {
    if let Some(context) = context {
        store::clear_answer_for_run_dir(&context.run_dir).await?;
        if let Some(state) = state
            && state.active_task_id.as_deref() == Some(context.task_id.as_str())
        {
            store::clear_answer().await?;
        }
        return Ok(());
    }
    store::clear_answer().await
}

fn should_use_legacy_state(
    state: Option<&StateData>,
    context: Option<&RuntimeTaskContext>,
) -> bool {
    context.is_none()
        || state.is_some_and(|state| {
            context.is_some_and(|context| {
                state.active_task_id.as_deref() == Some(context.task_id.as_str())
            })
        })
}

fn is_supervisor(agent_id: &str) -> bool {
    agent_id.starts_with("supervisor:")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use tempfile::TempDir;

    async fn setup() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ferrus/tasks")).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        tokio::fs::write(
            "ferrus.toml",
            "[checks]\ncommands = []\n\n[limits]\nmax_check_retries = 20\nmax_review_cycles = 3\nmax_feedback_lines = 30\nwait_timeout_secs = 1\n\n[lease]\nttl_secs = 60\n",
        )
        .await
        .unwrap();
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
    async fn ask_human_for_scoped_runtime_task_does_not_pause_unrelated_active_state() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        let mut state = StateData {
            state: TaskState::Executing,
            claimed_by: Some("executor:codex:1".to_string()),
            lease_until: Some(Utc::now() + chrono::Duration::seconds(60)),
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

        run("executor:codex:7", "Which path should I take?".to_string())
            .await
            .unwrap();

        let state = store::read_state().await.unwrap();
        assert_eq!(state.state, TaskState::Executing);
        assert_eq!(state.awaiting_human_by, None);
        assert_eq!(
            store::read_question_for_run_dir(".ferrus/runs/t-007")
                .await
                .unwrap(),
            "Which path should I take?"
        );
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/QUESTION.md")
                .await
                .unwrap_or_default(),
            ""
        );
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "awaiting_human");
        assert_eq!(task.paused_status.as_deref(), Some("executing"));
        assert_eq!(task.claimed_by.as_deref(), Some("executor:codex:7"));

        teardown(previous);
    }

    #[tokio::test]
    async fn ask_human_uses_database_context_when_state_json_is_absent() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status("t-007", ".ferrus/tasks/t-007.md", "executing")
            .await
            .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "executor:codex:7", 60)
            .await
            .unwrap();

        run("executor:codex:7", "Which path should I take?".to_string())
            .await
            .unwrap();

        assert!(store::read_state().await.is_err());
        assert_eq!(
            store::read_question_for_run_dir(".ferrus/runs/t-007")
                .await
                .unwrap(),
            "Which path should I take?"
        );
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "awaiting_human");
        assert_eq!(task.paused_status.as_deref(), Some("executing"));
        assert_eq!(
            crate::project::task_human_question_owner("t-007")
                .await
                .unwrap()
                .as_deref(),
            Some("executor:codex:7")
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn ask_human_mirrors_active_task_to_database_task() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        let mut state = StateData {
            state: TaskState::Addressing,
            claimed_by: Some("executor:codex:1".to_string()),
            lease_until: Some(Utc::now() + chrono::Duration::seconds(60)),
            ..Default::default()
        };
        state.set_active_task_artifacts(
            "t-001".to_string(),
            ".ferrus/tasks/t-001.md".to_string(),
            ".ferrus/runs/t-001".to_string(),
        );
        store::write_state(&state).await.unwrap();
        crate::project::record_task_status("t-001", ".ferrus/tasks/t-001.md", "addressing")
            .await
            .unwrap();
        crate::project::claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:1", 60)
            .await
            .unwrap();

        run("executor:codex:1", "Which path should I take?".to_string())
            .await
            .unwrap();

        let state = store::read_state().await.unwrap();
        assert_eq!(state.state, TaskState::AwaitingHuman);
        assert_eq!(state.paused_state, Some(TaskState::Addressing));
        assert_eq!(state.awaiting_human_by.as_deref(), Some("executor:codex:1"));
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-001").unwrap();
        assert_eq!(task.status, "awaiting_human");
        assert_eq!(task.paused_status.as_deref(), Some("addressing"));
        assert_eq!(task.claimed_by.as_deref(), Some("executor:codex:1"));

        teardown(previous);
    }
}
