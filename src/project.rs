use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::warn;

use crate::{agent_id::ENV_PROJECT_ROOT, platform, state::machine::TaskState};

const PROJECT_VERSION: u32 = 1;
const LOCAL_PROJECT_TOML: &str = ".ferrus/project.toml";
const CURRENT_TASK_ID: &str = "current";
const CURRENT_TASK_PATH: &str = ".ferrus/TASK.md";
static RUN_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LocalProjectRef {
    pub project_id: String,
    pub name: String,
    pub data_dir: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProjectMetadata {
    pub id: String,
    pub name: String,
    pub workspace_dir: String,
    pub ferrus_dir: String,
    pub vcs: Option<String>,
    pub origin_repo: Option<String>,
    pub default_branch: Option<String>,
    pub current_head: Option<String>,
    pub created_at: String,
    pub last_opened_at: String,
    pub version: u32,
}

#[derive(Debug)]
pub struct ProjectRegistration {
    pub local_ref: LocalProjectRef,
    pub metadata: ProjectMetadata,
    pub data_dir: PathBuf,
    pub database_path: PathBuf,
}

#[derive(Debug)]
pub struct DoctorReport {
    pub registration: ProjectRegistration,
    pub checks: Vec<DoctorCheck>,
}

#[derive(Debug)]
pub struct DoctorCheck {
    pub ok: bool,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct ProjectListEntry {
    pub id: String,
    pub name: Option<String>,
    pub workspace_dir: Option<String>,
    pub data_dir: PathBuf,
    pub database_exists: bool,
    pub last_opened_at: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeRecovery {
    pub interrupted_runs: usize,
    pub expired_task_leases: usize,
    pub state_lease_mirrors_cleared: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    pub id: String,
    pub task_id: String,
    pub role: String,
    pub agent: String,
    pub status: String,
    pub started_at: String,
    pub updated_at: String,
    pub pid: Option<u32>,
    pub workspace_path: String,
}

#[derive(Debug, Clone)]
pub struct EventRecord {
    pub id: i64,
    pub run_id: Option<String>,
    pub event_type: String,
    pub payload_json: String,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct TaskArtifact {
    pub id: String,
    pub path: String,
    pub run_dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRecord {
    pub id: String,
    pub path: String,
    pub spec_path: Option<String>,
    pub milestone_id: Option<String>,
    pub status: String,
    pub paused_status: Option<String>,
    pub claimed_by: Option<String>,
    pub lease_until: Option<String>,
    pub last_heartbeat: Option<String>,
    pub check_retries: u32,
    pub review_cycles: u32,
    pub failure_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HumanQuestion {
    pub task_id: String,
    pub task_path: String,
    pub run_dir: String,
    pub question: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct RuntimeTaskContext {
    pub task_id: String,
    pub task_path: String,
    pub run_dir: String,
    pub status: String,
    pub paused_status: Option<String>,
    pub check_retries: u32,
    pub review_cycles: u32,
    pub failure_reason: Option<String>,
    pub run_id: Option<String>,
    pub workspace_path: Option<String>,
}

#[derive(Debug, Clone)]
struct CurrentTaskRecord {
    id: String,
    path: String,
    spec_path: Option<String>,
    milestone_id: Option<String>,
}

#[derive(Debug, Clone)]
pub enum TaskClaim {
    Claimed {
        claimed_by: String,
        lease_until: DateTime<Utc>,
    },
    AlreadyClaimed {
        claimed_by: String,
        lease_until: DateTime<Utc>,
    },
    ClaimedByOther {
        claimed_by: String,
    },
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct TaskLease {
    pub task_id: String,
    pub task_path: String,
    pub status: String,
    pub paused_status: Option<String>,
    pub check_retries: u32,
    pub review_cycles: u32,
    pub failure_reason: Option<String>,
    pub claimed_by: String,
    pub lease_until: DateTime<Utc>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum ReadyTaskClaim {
    Claimed(TaskLease),
    AlreadyClaimed(TaskLease),
    NoAvailable,
}

#[derive(Debug, Clone)]
pub enum LeaseRenewal {
    Renewed {
        task_id: String,
        task_path: String,
        claimed_by: String,
        lease_until: DateTime<Utc>,
    },
    NotClaimed,
    ClaimedByOther {
        claimed_by: String,
    },
    Expired,
}

#[derive(Debug, Clone)]
pub enum TaskCheckFailure {
    Failed { retries: u32 },
    LimitExceeded { retries: u32 },
}

#[derive(Debug, Clone)]
pub enum TaskReviewRejection {
    Addressing { cycles: u32 },
    LimitExceeded { cycles: u32 },
}

#[derive(Debug, Clone)]
pub enum TaskConsultRestore {
    Restored { status: String },
    NotInConsultation,
}

#[derive(Debug, Clone)]
pub enum TaskHumanAnswerRestore {
    Restored { status: String },
    NotAwaitingHuman,
}

impl DoctorReport {
    pub fn has_errors(&self) -> bool {
        self.checks.iter().any(|check| !check.ok)
    }
}

pub async fn ensure_global_dir() -> Result<PathBuf> {
    let root = global_dir()?;
    tokio::fs::create_dir_all(root.join("projects"))
        .await
        .with_context(|| format!("Failed to create {}", root.join("projects").display()))?;
    Ok(root)
}

pub async fn register_current_project() -> Result<ProjectRegistration> {
    ensure_global_dir().await?;
    let workspace_dir = canonical_current_dir()
        .await
        .context("Failed to resolve current workspace directory")?;
    let ferrus_dir = workspace_dir.join(".ferrus");
    tokio::fs::create_dir_all(&ferrus_dir)
        .await
        .with_context(|| format!("Failed to create {}", ferrus_dir.display()))?;

    let now = timestamp();
    let existing = read_local_project_ref().await.ok();
    let project_id = if let Some(project) = existing.as_ref() {
        validate_project_id(&project.project_id)?;
        project.project_id.clone()
    } else {
        generate_project_id(&workspace_dir)
    };
    let data_dir = project_data_dir(&project_id)?;
    tokio::fs::create_dir_all(data_dir.join("logs"))
        .await
        .with_context(|| format!("Failed to create {}", data_dir.join("logs").display()))?;

    let project_toml_path = data_dir.join("project.toml");
    let previous_metadata = read_project_metadata_from(&project_toml_path).await.ok();
    let created_at = previous_metadata
        .as_ref()
        .map(|metadata| metadata.created_at.clone())
        .unwrap_or_else(|| now.clone());
    let name = workspace_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("project")
        .to_string();
    let git = read_git_metadata().await;
    let metadata = ProjectMetadata {
        id: project_id.clone(),
        name: name.clone(),
        workspace_dir: path_string(&workspace_dir),
        ferrus_dir: path_string(&ferrus_dir),
        vcs: git.as_ref().map(|_| "git".to_string()),
        origin_repo: git.as_ref().and_then(|git| git.origin_repo.clone()),
        default_branch: git.as_ref().and_then(|git| git.default_branch.clone()),
        current_head: git.as_ref().and_then(|git| git.current_head.clone()),
        created_at,
        last_opened_at: now,
        version: PROJECT_VERSION,
    };
    write_toml(&project_toml_path, &metadata).await?;

    let local_ref = LocalProjectRef {
        project_id,
        name,
        data_dir: path_string(&data_dir),
    };
    write_toml(&project_path(LOCAL_PROJECT_TOML), &local_ref).await?;

    let database_path = data_dir.join("ferrus.db");
    initialize_database(&database_path).await?;

    Ok(ProjectRegistration {
        local_ref,
        metadata,
        data_dir,
        database_path,
    })
}

pub async fn migrate_current_project() -> Result<ProjectRegistration> {
    let registration = register_current_project().await?;
    tokio::fs::create_dir_all(".ferrus/tasks")
        .await
        .context("Failed to create .ferrus/tasks")?;
    tokio::fs::create_dir_all(".ferrus/runs")
        .await
        .context("Failed to create .ferrus/runs")?;
    copy_legacy_artifacts().await?;
    if let Ok(mut state) = crate::state::store::read_state().await {
        if populate_legacy_active_artifacts(&mut state) {
            crate::state::store::write_state(&state).await?;
        }
        record_current_task_status_best_effort(task_status_for_state(&state.state)).await;
    }
    Ok(registration)
}

pub async fn touch_current_project() -> Result<ProjectRegistration> {
    let local_ref = read_local_project_ref()
        .await
        .context(".ferrus/project.toml not found or invalid — run `ferrus migrate`")?;
    validate_project_id(&local_ref.project_id)?;
    let data_dir = PathBuf::from(&local_ref.data_dir);
    tokio::fs::create_dir_all(data_dir.join("logs"))
        .await
        .with_context(|| format!("Failed to create {}", data_dir.join("logs").display()))?;

    let metadata_path = data_dir.join("project.toml");
    let previous_metadata = read_project_metadata_from(&metadata_path)
        .await
        .with_context(|| format!("Failed to read {}", metadata_path.display()))?;
    if previous_metadata.id != local_ref.project_id {
        anyhow::bail!(
            "local project_id {} does not match global metadata id {}",
            local_ref.project_id,
            previous_metadata.id
        );
    }

    let workspace_dir = canonical_current_dir()
        .await
        .context("Failed to resolve current workspace directory")?;
    let ferrus_dir = workspace_dir.join(".ferrus");
    let name = workspace_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("project")
        .to_string();
    let git = read_git_metadata().await;
    let metadata = ProjectMetadata {
        id: local_ref.project_id.clone(),
        name,
        workspace_dir: path_string(&workspace_dir),
        ferrus_dir: path_string(&ferrus_dir),
        vcs: git.as_ref().map(|_| "git".to_string()),
        origin_repo: git.as_ref().and_then(|git| git.origin_repo.clone()),
        default_branch: git.as_ref().and_then(|git| git.default_branch.clone()),
        current_head: git.as_ref().and_then(|git| git.current_head.clone()),
        created_at: previous_metadata.created_at,
        last_opened_at: timestamp(),
        version: PROJECT_VERSION,
    };
    write_toml(&metadata_path, &metadata).await?;
    initialize_database(&data_dir.join("ferrus.db")).await?;

    Ok(ProjectRegistration {
        local_ref,
        metadata,
        database_path: data_dir.join("ferrus.db"),
        data_dir,
    })
}

pub async fn allocate_task_artifact() -> Result<TaskArtifact> {
    let tasks_dir = Path::new(".ferrus/tasks");
    let runs_dir = Path::new(".ferrus/runs");
    tokio::fs::create_dir_all(tasks_dir)
        .await
        .context("Failed to create .ferrus/tasks")?;
    tokio::fs::create_dir_all(runs_dir)
        .await
        .context("Failed to create .ferrus/runs")?;

    let mut max_number = max_task_number_from_files(tasks_dir).await?;
    if let Ok(database_path) = current_database_path().await {
        max_number = max_number.max(max_task_number_from_database(&database_path).await?);
    }

    let mut number = max_number + 1;
    loop {
        let id = format!("t-{number:03}");
        let task_path = tasks_dir.join(format!("{id}.md"));
        if !task_path.exists() {
            // Store project-local artifact paths with `/` separators. Rust accepts these paths on
            // Windows too, and keeping the serialized STATE/DB value stable avoids platform drift.
            return Ok(TaskArtifact {
                path: format!(".ferrus/tasks/{id}.md"),
                run_dir: format!(".ferrus/runs/{id}"),
                id,
            });
        }
        number += 1;
    }
}

pub async fn doctor_current_project() -> Result<DoctorReport> {
    let local_ref = read_local_project_ref()
        .await
        .context(".ferrus/project.toml not found or invalid — run `ferrus migrate`")?;
    let data_dir = PathBuf::from(&local_ref.data_dir);
    let metadata_path = data_dir.join("project.toml");
    let metadata = read_project_metadata_from(&metadata_path)
        .await
        .with_context(|| format!("Failed to read {}", metadata_path.display()))?;
    let database_path = data_dir.join("ferrus.db");
    let current_dir = canonical_current_dir().await?;
    let current_ferrus_dir = current_dir.join(".ferrus");
    let expected_data_dir = project_data_dir(&local_ref.project_id)?;

    let mut checks = Vec::new();
    checks.push(DoctorCheck {
        ok: local_ref.project_id == metadata.id,
        message: format!(
            "local project_id matches global metadata id ({})",
            local_ref.project_id
        ),
    });
    checks.push(DoctorCheck {
        ok: equivalent_paths(&data_dir, &expected_data_dir).await,
        message: format!("data_dir points at {}", expected_data_dir.display()),
    });
    checks.push(DoctorCheck {
        ok: equivalent_paths(Path::new(&metadata.workspace_dir), &current_dir).await,
        message: format!("workspace_dir points at {}", current_dir.display()),
    });
    checks.push(DoctorCheck {
        ok: equivalent_paths(Path::new(&metadata.ferrus_dir), &current_ferrus_dir).await,
        message: format!("ferrus_dir points at {}", current_ferrus_dir.display()),
    });
    checks.push(DoctorCheck {
        ok: tokio::fs::metadata(&database_path).await.is_ok(),
        message: format!("database exists at {}", database_path.display()),
    });
    checks.push(DoctorCheck {
        ok: validate_database_schema(&database_path)
            .await
            .unwrap_or(false),
        message: "database has tasks, runs, events, and task lease columns".to_string(),
    });
    add_recovery_doctor_checks(&mut checks, &database_path).await;
    add_runtime_doctor_checks(&mut checks, &database_path).await;

    Ok(DoctorReport {
        registration: ProjectRegistration {
            local_ref,
            metadata,
            data_dir,
            database_path,
        },
        checks,
    })
}

async fn add_recovery_doctor_checks(checks: &mut Vec<DoctorCheck>, database_path: &Path) {
    let recovery = match preview_runtime_recovery_from(database_path).await {
        Ok(recovery) => recovery,
        Err(err) => {
            checks.push(DoctorCheck {
                ok: false,
                message: format!("runtime recovery preview can read ferrus.db ({err})"),
            });
            return;
        }
    };

    checks.push(DoctorCheck {
        ok: recovery.interrupted_runs == 0,
        message: format!(
            "no interrupted run recovery pending ({} found; run `ferrus recover`)",
            recovery.interrupted_runs
        ),
    });
    checks.push(DoctorCheck {
        ok: recovery.expired_task_leases == 0,
        message: format!(
            "no expired task lease recovery pending ({} found; run `ferrus recover`)",
            recovery.expired_task_leases
        ),
    });
    checks.push(DoctorCheck {
        ok: recovery.state_lease_mirrors_cleared == 0,
        message: format!(
            "no stale STATE.json lease mirror recovery pending ({} found; run `ferrus recover`)",
            recovery.state_lease_mirrors_cleared
        ),
    });
}

async fn add_runtime_doctor_checks(checks: &mut Vec<DoctorCheck>, database_path: &Path) {
    let state = match crate::state::store::read_state().await {
        Ok(state) => {
            checks.push(DoctorCheck {
                ok: true,
                message: "STATE.json is readable".to_string(),
            });
            state
        }
        Err(err) => {
            checks.push(DoctorCheck {
                ok: false,
                message: format!("STATE.json is readable ({err})"),
            });
            return;
        }
    };

    let active_fields = [
        state.active_task_id.as_ref(),
        state.active_task_path.as_ref(),
        state.active_run_dir.as_ref(),
    ];
    let active_field_count = active_fields.iter().filter(|field| field.is_some()).count();
    let active_metadata_complete = active_field_count == 0 || active_field_count == 3;
    checks.push(DoctorCheck {
        ok: active_metadata_complete,
        message: "active task metadata is complete when present".to_string(),
    });

    if state.state == TaskState::Idle {
        checks.push(DoctorCheck {
            ok: active_field_count == 0,
            message: "Idle STATE.json has no active task artifacts".to_string(),
        });
    } else {
        checks.push(DoctorCheck {
            ok: active_field_count == 3,
            message: format!("{:?} STATE.json has active task artifacts", state.state),
        });
    }

    if let Some(task_path) = state.active_task_path.as_deref() {
        checks.push(DoctorCheck {
            ok: tokio::fs::metadata(task_path).await.is_ok(),
            message: format!("active task path exists at {task_path}"),
        });
    }
    if let Some(run_dir) = state.active_run_dir.as_deref() {
        let run_dir_exists = tokio::fs::metadata(run_dir)
            .await
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false);
        checks.push(DoctorCheck {
            ok: run_dir_exists,
            message: format!("active run directory exists at {run_dir}"),
        });
    }

    let Some(task_id) = state.active_task_id.as_deref() else {
        return;
    };
    let task_row = match read_task_record_from_database(database_path, task_id).await {
        Ok(row) => row,
        Err(err) => {
            checks.push(DoctorCheck {
                ok: false,
                message: format!("active task row can be read from ferrus.db ({err})"),
            });
            return;
        }
    };
    let Some(task_row) = task_row else {
        checks.push(DoctorCheck {
            ok: false,
            message: format!("active task row exists in ferrus.db for {task_id}"),
        });
        return;
    };

    checks.push(DoctorCheck {
        ok: true,
        message: format!("active task row exists in ferrus.db for {task_id}"),
    });
    if let Some(active_task_path) = state.active_task_path.as_deref() {
        checks.push(DoctorCheck {
            ok: task_row.path == active_task_path,
            message: format!("active task DB path matches STATE.json ({active_task_path})"),
        });
    }
    if !matches!(
        state.state,
        TaskState::Consultation | TaskState::AwaitingHuman
    ) {
        let expected_status = task_status_for_state(&state.state);
        checks.push(DoctorCheck {
            ok: task_row.status == expected_status,
            message: format!("active task DB status matches STATE.json ({expected_status})"),
        });
    }
    checks.push(DoctorCheck {
        ok: task_row.claimed_by == state.claimed_by,
        message: "active task DB claim owner matches STATE.json".to_string(),
    });
}

pub async fn list_registered_projects() -> Result<Vec<ProjectListEntry>> {
    let projects_dir = global_dir()?.join("projects");
    list_registered_projects_from(&projects_dir).await
}

async fn list_registered_projects_from(projects_dir: &Path) -> Result<Vec<ProjectListEntry>> {
    if tokio::fs::metadata(projects_dir).await.is_err() {
        return Ok(Vec::new());
    }

    let mut entries = Vec::new();
    let mut read_dir = tokio::fs::read_dir(projects_dir)
        .await
        .with_context(|| format!("Failed to read {}", projects_dir.display()))?;
    while let Some(entry) = read_dir
        .next_entry()
        .await
        .with_context(|| format!("Failed to iterate {}", projects_dir.display()))?
    {
        let file_type = entry
            .file_type()
            .await
            .with_context(|| format!("Failed to inspect {}", entry.path().display()))?;
        if !file_type.is_dir() {
            continue;
        }

        let data_dir = entry.path();
        let fallback_id = entry.file_name().to_string_lossy().into_owned();
        let database_exists = tokio::fs::metadata(data_dir.join("ferrus.db"))
            .await
            .is_ok();
        match read_project_metadata_from(&data_dir.join("project.toml")).await {
            Ok(metadata) => entries.push(ProjectListEntry {
                id: metadata.id,
                name: Some(metadata.name),
                workspace_dir: Some(metadata.workspace_dir),
                data_dir,
                database_exists,
                last_opened_at: Some(metadata.last_opened_at),
                error: None,
            }),
            Err(err) => entries.push(ProjectListEntry {
                id: fallback_id,
                name: None,
                workspace_dir: None,
                data_dir,
                database_exists,
                last_opened_at: None,
                error: Some(err.to_string()),
            }),
        }
    }

    entries.sort_by(|left, right| {
        right
            .last_opened_at
            .cmp(&left.last_opened_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(entries)
}

pub async fn list_tasks() -> Result<Vec<TaskRecord>> {
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || -> Result<Vec<TaskRecord>> {
        let connection = open_runtime_database(&database_path)?;
        let mut statement = connection.prepare(
            r#"
            SELECT id, path, spec_path, milestone_id, status, paused_status, claimed_by,
                   lease_until, last_heartbeat, check_retries, review_cycles, failure_reason
            FROM tasks
            ORDER BY
                CASE WHEN id = 'current' THEN 0 ELSE 1 END,
                id
            "#,
        )?;
        let rows = statement.query_map([], task_record_from_row)?;

        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row?);
        }
        Ok(tasks)
    })
    .await?
}

pub async fn list_human_questions() -> Result<Vec<HumanQuestion>> {
    let tasks = list_tasks().await?;
    let mut questions = Vec::new();
    for task in tasks
        .into_iter()
        .filter(|task| task.status == "awaiting_human")
    {
        let run_dir = run_dir_for_task(&task.id);
        let question = crate::state::store::read_question_for_run_dir(&run_dir)
            .await
            .unwrap_or_default()
            .trim()
            .to_string();
        questions.push(HumanQuestion {
            task_id: task.id,
            task_path: task.path,
            run_dir,
            question,
        });
    }
    Ok(questions)
}

#[allow(dead_code)]
pub async fn find_non_terminal_task_by_origin(
    spec_path: &str,
    milestone_id: &str,
) -> Result<Option<TaskRecord>> {
    let database_path = current_database_path().await?;
    let spec_path = spec_path.to_string();
    let milestone_id = milestone_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<Option<TaskRecord>> {
        let connection = open_runtime_database(&database_path)?;
        let task = connection
            .query_row(
                r#"
                SELECT id, path, spec_path, milestone_id, status, paused_status, claimed_by,
                       lease_until, last_heartbeat, check_retries, review_cycles, failure_reason
                FROM tasks
                WHERE spec_path = ?1
                  AND milestone_id = ?2
                  AND status NOT IN ('idle', 'reset', 'complete', 'failed')
                ORDER BY id
                LIMIT 1
                "#,
                params![spec_path, milestone_id],
                task_record_from_row,
            )
            .optional()?;
        Ok(task)
    })
    .await?
}

pub async fn list_runs(limit: usize) -> Result<Vec<RunRecord>> {
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || -> Result<Vec<RunRecord>> {
        let connection = open_runtime_database(&database_path)?;
        let mut statement = connection.prepare(
            r#"
            SELECT id, task_id, role, agent, status, started_at, updated_at, pid, workspace_path
            FROM runs
            ORDER BY updated_at DESC, started_at DESC, id DESC
            LIMIT ?1
            "#,
        )?;
        let rows = statement.query_map([limit as i64], |row| {
            Ok(RunRecord {
                id: row.get(0)?,
                task_id: row.get(1)?,
                role: row.get(2)?,
                agent: row.get(3)?,
                status: row.get(4)?,
                started_at: row.get(5)?,
                updated_at: row.get(6)?,
                pid: row.get::<_, Option<i64>>(7)?.map(|pid| pid as u32),
                workspace_path: row.get(8)?,
            })
        })?;

        let mut runs = Vec::new();
        for row in rows {
            runs.push(row?);
        }
        Ok(runs)
    })
    .await?
}

pub async fn list_events(limit: usize, run_id: Option<String>) -> Result<Vec<EventRecord>> {
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || -> Result<Vec<EventRecord>> {
        let connection = open_runtime_database(&database_path)?;
        let mut events = Vec::new();
        if let Some(run_id) = run_id {
            let mut statement = connection.prepare(
                r#"
                SELECT id, run_id, type, payload_json, created_at
                FROM events
                WHERE run_id = ?1
                ORDER BY id DESC
                LIMIT ?2
                "#,
            )?;
            let rows = statement.query_map(params![run_id, limit as i64], event_from_row)?;
            for row in rows {
                events.push(row?);
            }
        } else {
            let mut statement = connection.prepare(
                r#"
                SELECT id, run_id, type, payload_json, created_at
                FROM events
                ORDER BY id DESC
                LIMIT ?1
                "#,
            )?;
            let rows = statement.query_map([limit as i64], event_from_row)?;
            for row in rows {
                events.push(row?);
            }
        }
        Ok(events)
    })
    .await?
}

fn event_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventRecord> {
    Ok(EventRecord {
        id: row.get(0)?,
        run_id: row.get(1)?,
        event_type: row.get(2)?,
        payload_json: row.get(3)?,
        created_at: row.get(4)?,
    })
}

fn task_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRecord> {
    Ok(TaskRecord {
        id: row.get(0)?,
        path: row.get(1)?,
        spec_path: row.get(2)?,
        milestone_id: row.get(3)?,
        status: row.get(4)?,
        paused_status: row.get(5)?,
        claimed_by: row.get(6)?,
        lease_until: row.get(7)?,
        last_heartbeat: row.get(8)?,
        check_retries: row.get::<_, i64>(9)? as u32,
        review_cycles: row.get::<_, i64>(10)? as u32,
        failure_reason: row.get(11)?,
    })
}

pub async fn record_current_task_status(status: &str) -> Result<()> {
    let task = current_task_record().await;
    record_task_status_with_origin(
        &task.id,
        &task.path,
        status,
        task.spec_path.as_deref(),
        task.milestone_id.as_deref(),
    )
    .await
}

pub async fn record_task_status(task_id: &str, task_path: &str, status: &str) -> Result<()> {
    record_task_status_with_origin(task_id, task_path, status, None, None).await
}

pub async fn record_task_status_with_origin(
    task_id: &str,
    task_path: &str,
    status: &str,
    spec_path: Option<&str>,
    milestone_id: Option<&str>,
) -> Result<()> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    let task_path = task_path.to_string();
    let status = status.to_string();
    let spec_path = spec_path.map(str::to_string);
    let milestone_id = milestone_id.map(str::to_string);
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        upsert_task(
            &connection,
            &task_id,
            &task_path,
            &status,
            spec_path.as_deref(),
            milestone_id.as_deref(),
        )?;
        if clears_task_lease_for_status(&status) {
            clear_task_lease(&connection, &task_id)?;
        }
        insert_event(
            &connection,
            None,
            "task_status_changed",
            &serde_json::json!({
                "task_id": task_id,
                "status": status,
            }),
        )?;
        Ok(())
    })
    .await?
}

pub async fn record_task_check_passed(task_id: &str) -> Result<()> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        connection.execute(
            "UPDATE tasks SET check_retries = 0, failure_reason = NULL WHERE id = ?1",
            [&task_id],
        )?;
        insert_event(
            &connection,
            None,
            "task_check_passed",
            &serde_json::json!({ "task_id": task_id }),
        )?;
        Ok(())
    })
    .await?
}

pub async fn record_task_integration_failed(
    task_id: &str,
    run_id: Option<&str>,
    failure_reason: &str,
) -> Result<()> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    let run_id = run_id.map(str::to_string);
    let failure_reason = failure_reason.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        connection.execute(
            "UPDATE tasks SET failure_reason = ?1 WHERE id = ?2",
            params![failure_reason, task_id],
        )?;
        insert_event(
            &connection,
            run_id.as_deref(),
            "task_integration_failed",
            &serde_json::json!({
                "task_id": task_id,
                "failure_reason": failure_reason,
            }),
        )?;
        Ok(())
    })
    .await?
}

pub async fn record_task_integration_failed_best_effort(
    task_id: &str,
    run_id: Option<&str>,
    failure_reason: &str,
) {
    if let Err(err) = record_task_integration_failed(task_id, run_id, failure_reason).await {
        warn!(error = ?err, task_id, "failed to mirror task integration failure into ferrus.db");
    }
}

pub async fn record_task_check_failed(
    task_id: &str,
    failure_reason: &str,
    max_retries: u32,
) -> Result<TaskCheckFailure> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    let failure_reason = failure_reason.to_string();
    tokio::task::spawn_blocking(move || -> Result<TaskCheckFailure> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction()?;
        let retries = task_check_retries(&transaction, &task_id)? + 1;
        if retries >= max_retries {
            let limit_failure_reason = format!(
                "Check failed {max_retries} consecutive times. Last failure:\n{failure_reason}"
            );
            transaction.execute(
                r#"
                UPDATE tasks
                SET status = 'failed', check_retries = ?1, failure_reason = ?2,
                    claimed_by = NULL, lease_until = NULL, last_heartbeat = NULL
                WHERE id = ?3
                "#,
                params![retries, limit_failure_reason, task_id],
            )?;
            insert_event_in_transaction(
                &transaction,
                None,
                "task_check_limit_exceeded",
                &serde_json::json!({
                    "task_id": task_id,
                    "retries": retries,
                    "max_retries": max_retries,
                }),
            )?;
            transaction.commit()?;
            Ok(TaskCheckFailure::LimitExceeded { retries })
        } else {
            transaction.execute(
                "UPDATE tasks SET check_retries = ?1, failure_reason = ?2 WHERE id = ?3",
                params![retries, failure_reason, task_id],
            )?;
            insert_event_in_transaction(
                &transaction,
                None,
                "task_check_failed",
                &serde_json::json!({
                    "task_id": task_id,
                    "retries": retries,
                    "max_retries": max_retries,
                }),
            )?;
            transaction.commit()?;
            Ok(TaskCheckFailure::Failed { retries })
        }
    })
    .await?
}

pub async fn record_task_review_rejected(
    task_id: &str,
    max_cycles: u32,
) -> Result<TaskReviewRejection> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<TaskReviewRejection> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction()?;
        let cycles = task_review_cycles(&transaction, &task_id)? + 1;
        if cycles >= max_cycles {
            transaction.execute(
                r#"
                UPDATE tasks
                SET status = 'failed', review_cycles = ?1,
                    failure_reason = ?2,
                    claimed_by = NULL, lease_until = NULL, last_heartbeat = NULL
                WHERE id = ?3
                "#,
                params![
                    cycles,
                    format!("Task rejected {max_cycles} times without resolution."),
                    task_id
                ],
            )?;
            insert_event_in_transaction(
                &transaction,
                None,
                "task_review_limit_exceeded",
                &serde_json::json!({
                    "task_id": task_id,
                    "review_cycles": cycles,
                    "max_review_cycles": max_cycles,
                }),
            )?;
            transaction.commit()?;
            Ok(TaskReviewRejection::LimitExceeded { cycles })
        } else {
            transaction.execute(
                r#"
                UPDATE tasks
                SET status = 'addressing', review_cycles = ?1, check_retries = 0,
                    failure_reason = NULL,
                    claimed_by = NULL, lease_until = NULL, last_heartbeat = NULL
                WHERE id = ?2
                "#,
                params![cycles, task_id],
            )?;
            insert_event_in_transaction(
                &transaction,
                None,
                "task_rejected",
                &serde_json::json!({
                    "task_id": task_id,
                    "review_cycles": cycles,
                    "max_review_cycles": max_cycles,
                }),
            )?;
            transaction.commit()?;
            Ok(TaskReviewRejection::Addressing { cycles })
        }
    })
    .await?
}

pub async fn record_task_consultation_requested(task_id: &str, paused_status: &str) -> Result<()> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    let paused_status = paused_status.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        connection.execute(
            "UPDATE tasks SET status = 'consultation', paused_status = ?1 WHERE id = ?2",
            params![paused_status, task_id],
        )?;
        insert_event(
            &connection,
            None,
            "task_consultation_requested",
            &serde_json::json!({
                "task_id": task_id,
                "paused_status": paused_status,
            }),
        )?;
        Ok(())
    })
    .await?
}

pub async fn restore_task_from_consultation(task_id: &str) -> Result<TaskConsultRestore> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<TaskConsultRestore> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction()?;
        let row = transaction
            .query_row(
                "SELECT status, paused_status FROM tasks WHERE id = ?1",
                [&task_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        let Some((status, paused_status)) = row else {
            transaction.commit()?;
            return Ok(TaskConsultRestore::NotInConsultation);
        };
        if status != "consultation" {
            transaction.commit()?;
            return Ok(TaskConsultRestore::NotInConsultation);
        }
        let resumed_status = paused_status.unwrap_or_else(|| "executing".to_string());
        transaction.execute(
            "UPDATE tasks SET status = ?1, paused_status = NULL WHERE id = ?2",
            params![resumed_status, task_id],
        )?;
        insert_event_in_transaction(
            &transaction,
            None,
            "task_consultation_resolved",
            &serde_json::json!({
                "task_id": task_id,
                "resumed_status": resumed_status,
            }),
        )?;
        transaction.commit()?;
        Ok(TaskConsultRestore::Restored {
            status: resumed_status,
        })
    })
    .await?
}

pub async fn record_task_human_question_requested(
    task_id: &str,
    paused_status: &str,
) -> Result<()> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    let paused_status = paused_status.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        connection.execute(
            "UPDATE tasks SET status = 'awaiting_human', paused_status = ?1 WHERE id = ?2",
            params![paused_status, task_id],
        )?;
        insert_event(
            &connection,
            None,
            "task_human_question_requested",
            &serde_json::json!({
                "task_id": task_id,
                "paused_status": paused_status,
            }),
        )?;
        Ok(())
    })
    .await?
}

pub async fn restore_task_from_human_answer(task_id: &str) -> Result<TaskHumanAnswerRestore> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<TaskHumanAnswerRestore> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction()?;
        let row = transaction
            .query_row(
                "SELECT status, paused_status FROM tasks WHERE id = ?1",
                [&task_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        let Some((status, paused_status)) = row else {
            transaction.commit()?;
            return Ok(TaskHumanAnswerRestore::NotAwaitingHuman);
        };
        if status != "awaiting_human" {
            transaction.commit()?;
            return Ok(TaskHumanAnswerRestore::NotAwaitingHuman);
        }
        let resumed_status = paused_status.unwrap_or_else(|| "executing".to_string());
        transaction.execute(
            "UPDATE tasks SET status = ?1, paused_status = NULL WHERE id = ?2",
            params![resumed_status, task_id],
        )?;
        insert_event_in_transaction(
            &transaction,
            None,
            "task_human_answered",
            &serde_json::json!({
                "task_id": task_id,
                "resumed_status": resumed_status,
            }),
        )?;
        transaction.commit()?;
        Ok(TaskHumanAnswerRestore::Restored {
            status: resumed_status,
        })
    })
    .await?
}

fn clears_task_lease_for_status(status: &str) -> bool {
    matches!(
        status,
        "idle" | "reset" | "reviewing" | "addressing" | "complete" | "failed"
    )
}

pub async fn claim_current_task(agent_id: &str, ttl_secs: u64) -> Result<TaskClaim> {
    let (task_id, task_path) = current_task_identity().await;
    claim_task(&task_id, &task_path, agent_id, ttl_secs).await
}

pub async fn claim_task(
    task_id: &str,
    task_path: &str,
    agent_id: &str,
    ttl_secs: u64,
) -> Result<TaskClaim> {
    let database_path = current_database_path().await?;
    claim_task_in_database(
        database_path,
        task_id.to_string(),
        task_path.to_string(),
        agent_id,
        ttl_secs,
    )
    .await
}

#[allow(dead_code)]
pub async fn claim_next_ready_task(agent_id: &str, ttl_secs: u64) -> Result<ReadyTaskClaim> {
    claim_next_task_with_statuses(agent_id, ttl_secs, &["executing", "addressing"]).await
}

pub async fn claim_ready_task_by_id(
    task_id: &str,
    agent_id: &str,
    ttl_secs: u64,
) -> Result<ReadyTaskClaim> {
    claim_task_by_id_with_statuses(
        task_id,
        agent_id,
        ttl_secs,
        &["pending", "executing", "addressing"],
        true,
    )
    .await
}

pub async fn claim_review_task_by_id(
    task_id: &str,
    agent_id: &str,
    ttl_secs: u64,
) -> Result<ReadyTaskClaim> {
    claim_task_by_id_with_statuses(task_id, agent_id, ttl_secs, &["reviewing"], false).await
}

async fn claim_task_by_id_with_statuses(
    task_id: &str,
    agent_id: &str,
    ttl_secs: u64,
    allowed_statuses: &[&str],
    promote_pending: bool,
) -> Result<ReadyTaskClaim> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    let agent_id = agent_id.to_string();
    let allowed_statuses = allowed_statuses
        .iter()
        .map(|status| status.to_string())
        .collect::<Vec<_>>();
    tokio::task::spawn_blocking(move || -> Result<ReadyTaskClaim> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction()?;
        let now = Utc::now();
        let Some(mut candidate) = task_candidate_by_id(&transaction, &task_id)? else {
            transaction.commit()?;
            return Ok(ReadyTaskClaim::NoAvailable);
        };

        if !allowed_statuses.iter().any(|status| status == &candidate.status) {
            transaction.commit()?;
            return Ok(ReadyTaskClaim::NoAvailable);
        }

        if promote_pending && candidate.status == "pending" {
            transaction.execute(
                "UPDATE tasks SET status = 'executing', paused_status = NULL WHERE id = ?1 AND status = 'pending'",
                [&candidate.id],
            )?;
            insert_event_in_transaction(
                &transaction,
                None,
                "task_scheduled",
                &serde_json::json!({
                    "task_id": candidate.id,
                    "previous_status": candidate.status,
                    "status": "executing",
                    "scheduled_at": timestamp(),
                }),
            )?;
            candidate.status = "executing".to_string();
            candidate.paused_status = None;
        }

        let lease_until = parse_lease_until(candidate.lease_until.as_deref());
        let lease_active = lease_until
            .as_ref()
            .is_some_and(|lease_until| now < *lease_until);
        if lease_active && candidate.claimed_by.as_deref() == Some(agent_id.as_str()) {
            transaction.commit()?;
            return Ok(ReadyTaskClaim::AlreadyClaimed(TaskLease {
                task_id: candidate.id,
                task_path: candidate.path,
                status: candidate.status,
                paused_status: candidate.paused_status,
                check_retries: candidate.check_retries,
                review_cycles: candidate.review_cycles,
                failure_reason: candidate.failure_reason,
                claimed_by: agent_id,
                lease_until: lease_until.expect("active lease exists"),
            }));
        }
        if lease_active {
            transaction.commit()?;
            return Ok(ReadyTaskClaim::NoAvailable);
        }

        let lease_until =
            now + chrono::Duration::try_seconds(ttl_secs as i64).unwrap_or(chrono::Duration::MAX);
        claim_task_in_transaction(&transaction, &candidate.id, &agent_id, lease_until, now)?;
        transaction.commit()?;
        Ok(ReadyTaskClaim::Claimed(TaskLease {
            task_id: candidate.id,
            task_path: candidate.path,
            status: candidate.status,
            paused_status: candidate.paused_status,
            check_retries: candidate.check_retries,
            review_cycles: candidate.review_cycles,
            failure_reason: candidate.failure_reason,
            claimed_by: agent_id,
            lease_until,
        }))
    })
    .await?
}

pub async fn claim_next_review_task(agent_id: &str, ttl_secs: u64) -> Result<ReadyTaskClaim> {
    claim_next_task_with_statuses(agent_id, ttl_secs, &["reviewing"]).await
}

async fn claim_next_task_with_statuses(
    agent_id: &str,
    ttl_secs: u64,
    statuses: &[&str],
) -> Result<ReadyTaskClaim> {
    let database_path = current_database_path().await?;
    let agent_id = agent_id.to_string();
    let statuses = statuses
        .iter()
        .map(|status| status.to_string())
        .collect::<Vec<_>>();
    tokio::task::spawn_blocking(move || -> Result<ReadyTaskClaim> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction()?;
        let now = Utc::now();
        let candidates = task_candidates_by_status(&transaction, &statuses)?;

        for candidate in &candidates {
            let lease_until = parse_lease_until(candidate.lease_until.as_deref());
            let lease_active = lease_until
                .as_ref()
                .is_some_and(|lease_until| now < *lease_until);
            if lease_active && candidate.claimed_by.as_deref() == Some(agent_id.as_str()) {
                transaction.commit()?;
                return Ok(ReadyTaskClaim::AlreadyClaimed(TaskLease {
                    task_id: candidate.id.clone(),
                    task_path: candidate.path.clone(),
                    status: candidate.status.clone(),
                    paused_status: candidate.paused_status.clone(),
                    check_retries: candidate.check_retries,
                    review_cycles: candidate.review_cycles,
                    failure_reason: candidate.failure_reason.clone(),
                    claimed_by: agent_id,
                    lease_until: lease_until.expect("active lease exists"),
                }));
            }
        }

        for candidate in candidates {
            let lease_until = parse_lease_until(candidate.lease_until.as_deref());
            let lease_active = lease_until
                .as_ref()
                .is_some_and(|lease_until| now < *lease_until);
            if lease_active {
                continue;
            }

            let lease_until = now
                + chrono::Duration::try_seconds(ttl_secs as i64).unwrap_or(chrono::Duration::MAX);
            claim_task_in_transaction(&transaction, &candidate.id, &agent_id, lease_until, now)?;
            transaction.commit()?;
            return Ok(ReadyTaskClaim::Claimed(TaskLease {
                task_id: candidate.id,
                task_path: candidate.path,
                status: candidate.status,
                paused_status: candidate.paused_status,
                check_retries: candidate.check_retries,
                review_cycles: candidate.review_cycles,
                failure_reason: candidate.failure_reason,
                claimed_by: agent_id,
                lease_until,
            }));
        }

        transaction.commit()?;
        Ok(ReadyTaskClaim::NoAvailable)
    })
    .await?
}

async fn claim_task_in_database(
    database_path: PathBuf,
    task_id: String,
    task_path: String,
    agent_id: &str,
    ttl_secs: u64,
) -> Result<TaskClaim> {
    let agent_id = agent_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<TaskClaim> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction()?;
        ensure_task_exists(&transaction, &task_id, &task_path)?;
        let existing: Option<(Option<String>, Option<String>)> = transaction
            .query_row(
                "SELECT claimed_by, lease_until FROM tasks WHERE id = ?1",
                [&task_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (claimed_by, lease_until) = existing.unwrap_or((None, None));
        let now = Utc::now();
        let existing_lease = lease_until
            .as_deref()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc));
        let lease_active = existing_lease.is_some_and(|lease_until| now < lease_until);

        if lease_active && claimed_by.as_deref() == Some(agent_id.as_str()) {
            transaction.commit()?;
            return Ok(TaskClaim::AlreadyClaimed {
                claimed_by: agent_id,
                lease_until: existing_lease.expect("active lease exists"),
            });
        }
        if lease_active {
            transaction.commit()?;
            return Ok(TaskClaim::ClaimedByOther {
                claimed_by: claimed_by.unwrap_or_else(|| "unknown".to_string()),
            });
        }

        let lease_until =
            now + chrono::Duration::try_seconds(ttl_secs as i64).unwrap_or(chrono::Duration::MAX);
        claim_task_in_transaction(&transaction, &task_id, &agent_id, lease_until, now)?;
        transaction.commit()?;
        Ok(TaskClaim::Claimed {
            claimed_by: agent_id,
            lease_until,
        })
    })
    .await?
}

pub async fn renew_current_task_lease(agent_id: &str, ttl_secs: u64) -> Result<LeaseRenewal> {
    let database_path = current_database_path().await?;
    let (task_id, task_path) = current_task_identity().await;
    let agent_id = agent_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<LeaseRenewal> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction()?;
        let existing: Option<(Option<String>, Option<String>)> = transaction
            .query_row(
                "SELECT claimed_by, lease_until FROM tasks WHERE id = ?1",
                [&task_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((claimed_by, lease_until)) = existing else {
            transaction.commit()?;
            return Ok(LeaseRenewal::NotClaimed);
        };
        let Some(claimed_by) = claimed_by else {
            transaction.commit()?;
            return Ok(LeaseRenewal::NotClaimed);
        };
        if claimed_by != agent_id {
            transaction.commit()?;
            return Ok(LeaseRenewal::ClaimedByOther { claimed_by });
        }
        let Some(lease_until) = renew_task_lease_in_transaction(
            &transaction,
            &task_id,
            &agent_id,
            ttl_secs,
            lease_until.as_deref(),
        )?
        else {
            transaction.commit()?;
            return Ok(LeaseRenewal::Expired);
        };
        transaction.commit()?;
        Ok(LeaseRenewal::Renewed {
            task_id,
            task_path,
            claimed_by: agent_id,
            lease_until,
        })
    })
    .await?
}

pub async fn renew_claimed_task_lease(agent_id: &str, ttl_secs: u64) -> Result<LeaseRenewal> {
    let database_path = current_database_path().await?;
    let agent_id = agent_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<LeaseRenewal> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction()?;
        let existing: Option<(String, String, Option<String>)> = transaction
            .query_row(
                r#"
                SELECT id, path, lease_until
                FROM tasks
                WHERE claimed_by = ?1
                ORDER BY
                    CASE WHEN lease_until IS NULL THEN 1 ELSE 0 END,
                    lease_until DESC,
                    id
                LIMIT 1
                "#,
                [&agent_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((task_id, task_path, lease_until)) = existing else {
            transaction.commit()?;
            return Ok(LeaseRenewal::NotClaimed);
        };

        let Some(lease_until) = renew_task_lease_in_transaction(
            &transaction,
            &task_id,
            &agent_id,
            ttl_secs,
            lease_until.as_deref(),
        )?
        else {
            transaction.commit()?;
            return Ok(LeaseRenewal::Expired);
        };
        transaction.commit()?;
        Ok(LeaseRenewal::Renewed {
            task_id,
            task_path,
            claimed_by: agent_id,
            lease_until,
        })
    })
    .await?
}

pub async fn runtime_task_context_for_agent(agent_id: &str) -> Result<Option<RuntimeTaskContext>> {
    let database_path = current_database_path().await?;
    let agent_id = agent_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<Option<RuntimeTaskContext>> {
        let connection = open_runtime_database(&database_path)?;
        if let Some((
            task_id,
            task_path,
            status,
            paused_status,
            check_retries,
            review_cycles,
            failure_reason,
        )) = connection
            .query_row(
                r#"
                SELECT id, path, status, paused_status,
                       check_retries, review_cycles, failure_reason
                FROM tasks
                WHERE claimed_by = ?1
                ORDER BY
                    CASE WHEN lease_until IS NULL THEN 1 ELSE 0 END,
                    lease_until DESC,
                    id
                LIMIT 1
                "#,
                [&agent_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, i64>(4)? as u32,
                        row.get::<_, i64>(5)? as u32,
                        row.get::<_, Option<String>>(6)?,
                    ))
                },
            )
            .optional()?
        {
            let run = latest_run_for_agent_task(&connection, &agent_id, &task_id)?;
            return Ok(Some(RuntimeTaskContext {
                run_dir: run_dir_for_task(&task_id),
                task_id,
                task_path,
                status,
                paused_status,
                check_retries,
                review_cycles,
                failure_reason,
                run_id: run.as_ref().map(|run| run.id.clone()),
                workspace_path: run.map(|run| run.workspace_path),
            }));
        }

        let context = connection
            .query_row(
                r#"
                SELECT runs.id, runs.workspace_path,
                       tasks.id, tasks.path, tasks.status, tasks.paused_status,
                       tasks.check_retries, tasks.review_cycles, tasks.failure_reason
                FROM runs
                JOIN tasks ON tasks.id = runs.task_id
                WHERE runs.agent = ?1 AND runs.status IN ('running', 'checking', 'reviewing')
                ORDER BY runs.updated_at DESC, runs.started_at DESC, runs.id DESC
                LIMIT 1
                "#,
                [&agent_id],
                |row| {
                    let run_id = row.get::<_, String>(0)?;
                    let workspace_path = row.get::<_, String>(1)?;
                    let task_id = row.get::<_, String>(2)?;
                    Ok(RuntimeTaskContext {
                        run_dir: run_dir_for_task(&task_id),
                        task_id,
                        task_path: row.get(3)?,
                        status: row.get(4)?,
                        paused_status: row.get(5)?,
                        check_retries: row.get::<_, i64>(6)? as u32,
                        review_cycles: row.get::<_, i64>(7)? as u32,
                        failure_reason: row.get(8)?,
                        run_id: Some(run_id),
                        workspace_path: Some(workspace_path),
                    })
                },
            )
            .optional()?;
        Ok(context)
    })
    .await?
}

fn renew_task_lease_in_transaction(
    transaction: &Transaction<'_>,
    task_id: &str,
    agent_id: &str,
    ttl_secs: u64,
    existing_lease: Option<&str>,
) -> Result<Option<DateTime<Utc>>> {
    let now = Utc::now();
    let existing_lease = existing_lease
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc));
    if existing_lease.is_none_or(|lease_until| now >= lease_until) {
        return Ok(None);
    }

    let lease_until =
        now + chrono::Duration::try_seconds(ttl_secs as i64).unwrap_or(chrono::Duration::MAX);
    let lease_until_text = lease_until.to_rfc3339_opts(SecondsFormat::Secs, true);
    let now_text = now.to_rfc3339_opts(SecondsFormat::Secs, true);
    transaction.execute(
        "UPDATE tasks SET lease_until = ?1, last_heartbeat = ?2 WHERE id = ?3",
        params![lease_until_text, now_text, task_id],
    )?;
    insert_event_in_transaction(
        transaction,
        None,
        "task_lease_renewed",
        &serde_json::json!({
            "task_id": task_id,
            "claimed_by": agent_id,
            "lease_until": lease_until,
        }),
    )?;
    Ok(Some(lease_until))
}

#[derive(Debug, Clone)]
struct RuntimeRunIdentity {
    id: String,
    workspace_path: String,
}

fn latest_run_for_agent_task(
    connection: &Connection,
    agent_id: &str,
    task_id: &str,
) -> Result<Option<RuntimeRunIdentity>> {
    Ok(connection
        .query_row(
            r#"
            SELECT id, workspace_path
            FROM runs
            WHERE agent = ?1 AND task_id = ?2
            ORDER BY updated_at DESC, started_at DESC, id DESC
            LIMIT 1
            "#,
            params![agent_id, task_id],
            |row| {
                Ok(RuntimeRunIdentity {
                    id: row.get(0)?,
                    workspace_path: row.get(1)?,
                })
            },
        )
        .optional()?)
}

fn latest_active_run_for_agent(connection: &Connection, agent_id: &str) -> Result<Option<String>> {
    Ok(connection
        .query_row(
            r#"
            SELECT id
            FROM runs
            WHERE agent = ?1 AND status IN ('running', 'checking', 'reviewing')
            ORDER BY updated_at DESC, started_at DESC, id DESC
            LIMIT 1
            "#,
            [agent_id],
            |row| row.get(0),
        )
        .optional()?)
}

fn consultation_context_for_run(
    connection: &Connection,
    run_id: &str,
) -> Result<Option<RuntimeTaskContext>> {
    Ok(connection
        .query_row(
            r#"
            SELECT tasks.id, tasks.path, tasks.status, tasks.paused_status,
                   tasks.check_retries, tasks.review_cycles, tasks.failure_reason
            FROM runs
            JOIN tasks ON tasks.id = runs.task_id
            WHERE runs.id = ?1 AND tasks.status = 'consultation'
            LIMIT 1
            "#,
            [run_id],
            |row| {
                let task_id = row.get::<_, String>(0)?;
                Ok(RuntimeTaskContext {
                    run_dir: run_dir_for_task(&task_id),
                    task_id,
                    task_path: row.get(1)?,
                    status: row.get(2)?,
                    paused_status: row.get(3)?,
                    check_retries: row.get::<_, i64>(4)? as u32,
                    review_cycles: row.get::<_, i64>(5)? as u32,
                    failure_reason: row.get(6)?,
                    run_id: Some(run_id.to_string()),
                    workspace_path: None,
                })
            },
        )
        .optional()?)
}

fn run_dir_for_task(task_id: &str) -> String {
    format!(".ferrus/runs/{task_id}")
}

pub async fn record_current_task_status_best_effort(status: &str) {
    if let Err(err) = record_current_task_status(status).await {
        warn!(error = ?err, status, "failed to mirror task status into ferrus.db");
    }
}

pub async fn record_task_status_best_effort(task_id: &str, task_path: &str, status: &str) {
    if let Err(err) = record_task_status(task_id, task_path, status).await {
        warn!(error = ?err, task_id, status, "failed to mirror task status into ferrus.db");
    }
}

pub async fn record_runtime_event(
    run_id: Option<String>,
    event_type: &str,
    payload: serde_json::Value,
) -> Result<()> {
    let database_path = current_database_path().await?;
    let event_type = event_type.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        insert_event(&connection, run_id.as_deref(), &event_type, &payload)?;
        Ok(())
    })
    .await?
}

pub async fn record_runtime_event_best_effort(
    run_id: Option<String>,
    event_type: &str,
    payload: serde_json::Value,
) {
    if let Err(err) = record_runtime_event(run_id, event_type, payload).await {
        warn!(error = ?err, event_type, "failed to write runtime event into ferrus.db");
    }
}

pub fn allocate_run_id(role: &str, agent: &str) -> String {
    generate_run_id(role, agent)
}

#[cfg(test)]
pub async fn record_run_started(role: &str, agent: &str, pid: u32) -> Result<RunRecord> {
    let run_id = allocate_run_id(role, agent);
    record_run_started_with_id(&run_id, role, agent, pid).await
}

#[cfg(test)]
pub async fn record_run_started_with_id(
    run_id: &str,
    role: &str,
    agent: &str,
    pid: u32,
) -> Result<RunRecord> {
    let workspace_path = path_string(&canonical_current_dir().await?);
    record_run_started_with_workspace(run_id, role, agent, pid, workspace_path).await
}

pub async fn record_run_started_with_workspace(
    run_id: &str,
    role: &str,
    agent: &str,
    pid: u32,
    workspace_path: String,
) -> Result<RunRecord> {
    let database_path = current_database_path().await?;
    let (task_id, task_path) = current_task_identity().await;
    let run_id = run_id.to_string();
    let role = role.to_string();
    let agent = agent.to_string();
    let started_at = timestamp();
    let updated_at = started_at.clone();
    let record = RunRecord {
        id: run_id.clone(),
        task_id: task_id.clone(),
        role,
        agent,
        status: "running".to_string(),
        started_at: started_at.clone(),
        updated_at: updated_at.clone(),
        pid: Some(pid),
        workspace_path,
    };
    let record_for_insert = record.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        ensure_task_exists(&connection, &task_id, &task_path)?;
        connection.execute(
            r#"
            INSERT INTO runs (
                id, task_id, role, agent, status, started_at, updated_at, pid, workspace_path
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            ON CONFLICT(id) DO UPDATE SET
                status = excluded.status,
                updated_at = excluded.updated_at,
                pid = excluded.pid,
                workspace_path = excluded.workspace_path
            "#,
            params![
                record_for_insert.id,
                record_for_insert.task_id,
                record_for_insert.role,
                record_for_insert.agent,
                record_for_insert.status,
                started_at,
                updated_at,
                record_for_insert.pid.map(i64::from),
                record_for_insert.workspace_path,
            ],
        )?;
        insert_event(
            &connection,
            Some(&run_id),
            "run_started",
            &serde_json::json!({
                "role": record_for_insert.role,
                "agent": record_for_insert.agent,
                "pid": record_for_insert.pid,
            }),
        )?;
        Ok(())
    })
    .await??;
    Ok(record)
}

pub async fn record_run_started_with_id_best_effort(
    run_id: &str,
    role: &str,
    agent: &str,
    pid: u32,
    workspace_path: String,
) -> Option<String> {
    match record_run_started_with_workspace(run_id, role, agent, pid, workspace_path).await {
        Ok(record) => Some(record.id),
        Err(err) => {
            warn!(error = ?err, run_id, role, agent, pid, "failed to mirror run start into ferrus.db");
            None
        }
    }
}

pub async fn attach_running_run_to_task(
    agent_id: &str,
    task_id: &str,
    task_path: &str,
) -> Result<Option<String>> {
    let database_path = current_database_path().await?;
    let agent_id = agent_id.to_string();
    let task_id = task_id.to_string();
    let task_path = task_path.to_string();
    tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        let connection = open_runtime_database(&database_path)?;
        ensure_task_exists(&connection, &task_id, &task_path)?;
        let run_id: Option<String> = connection
            .query_row(
                r#"
                SELECT id
                FROM runs
                WHERE agent = ?1 AND status IN ('running', 'checking', 'reviewing')
                ORDER BY started_at DESC, id DESC
                LIMIT 1
                "#,
                [&agent_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(run_id) = run_id else {
            return Ok(None);
        };

        connection.execute(
            "UPDATE runs SET task_id = ?1, updated_at = ?2 WHERE id = ?3",
            params![task_id, timestamp(), run_id],
        )?;
        insert_event(
            &connection,
            Some(&run_id),
            "run_task_attached",
            &serde_json::json!({
                "agent": agent_id,
                "task_id": task_id,
            }),
        )?;
        Ok(Some(run_id))
    })
    .await?
}

pub async fn attach_running_run_to_task_best_effort(
    agent_id: &str,
    task_id: &str,
    task_path: &str,
) {
    if let Err(err) = attach_running_run_to_task(agent_id, task_id, task_path).await {
        warn!(
            error = ?err,
            agent_id,
            task_id,
            "failed to attach running run to task in ferrus.db"
        );
    }
}

pub async fn attach_running_run_to_next_consultation(
    agent_id: &str,
) -> Result<Option<RuntimeTaskContext>> {
    let database_path = current_database_path().await?;
    let agent_id = agent_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<Option<RuntimeTaskContext>> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(run_id) = latest_active_run_for_agent(&transaction, &agent_id)? else {
            transaction.commit()?;
            return Ok(None);
        };

        if let Some(context) = consultation_context_for_run(&transaction, &run_id)? {
            transaction.commit()?;
            return Ok(Some(context));
        }

        let candidate = transaction
            .query_row(
                r#"
                SELECT id, path, status, paused_status,
                       check_retries, review_cycles, failure_reason
                FROM tasks
                WHERE status = 'consultation'
                  AND NOT EXISTS (
                      SELECT 1
                      FROM runs
                      WHERE runs.task_id = tasks.id
                        AND runs.role = 'supervisor'
                        AND runs.status IN ('running', 'checking', 'reviewing')
                  )
                ORDER BY id
                LIMIT 1
                "#,
                [],
                |row| {
                    Ok(RuntimeTaskContext {
                        task_id: row.get(0)?,
                        task_path: row.get(1)?,
                        run_dir: String::new(),
                        status: row.get(2)?,
                        paused_status: row.get(3)?,
                        check_retries: row.get::<_, i64>(4)? as u32,
                        review_cycles: row.get::<_, i64>(5)? as u32,
                        failure_reason: row.get(6)?,
                        run_id: Some(run_id.clone()),
                        workspace_path: None,
                    })
                },
            )
            .optional()?;
        let Some(mut context) = candidate else {
            transaction.commit()?;
            return Ok(None);
        };

        context.run_dir = run_dir_for_task(&context.task_id);
        let attached = transaction.execute(
            r#"
            UPDATE runs
            SET task_id = ?1, updated_at = ?2
            WHERE id = ?3
              AND NOT EXISTS (
                  SELECT 1
                  FROM runs active_runs
                  WHERE active_runs.task_id = ?1
                    AND active_runs.role = 'supervisor'
                    AND active_runs.status IN ('running', 'checking', 'reviewing')
                    AND active_runs.id <> ?3
              )
            "#,
            params![context.task_id, timestamp(), run_id],
        )?;
        if attached == 0 {
            transaction.commit()?;
            return Ok(None);
        }
        insert_event_in_transaction(
            &transaction,
            Some(&run_id),
            "run_consultation_attached",
            &serde_json::json!({
                "agent": agent_id,
                "task_id": context.task_id,
            }),
        )?;
        transaction.commit()?;
        Ok(Some(context))
    })
    .await?
}

pub async fn attach_running_run_to_consultation(
    task_id: &str,
    agent_id: &str,
) -> Result<Option<RuntimeTaskContext>> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    let agent_id = agent_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<Option<RuntimeTaskContext>> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(run_id) = latest_active_run_for_agent(&transaction, &agent_id)? else {
            transaction.commit()?;
            return Ok(None);
        };

        if let Some(context) = consultation_context_for_run(&transaction, &run_id)? {
            transaction.commit()?;
            return Ok((context.task_id == task_id).then_some(context));
        }

        let candidate = transaction
            .query_row(
                r#"
                SELECT id, path, status, paused_status,
                       check_retries, review_cycles, failure_reason
                FROM tasks
                WHERE id = ?1
                  AND status = 'consultation'
                  AND NOT EXISTS (
                      SELECT 1
                      FROM runs
                      WHERE runs.task_id = tasks.id
                        AND runs.role = 'supervisor'
                        AND runs.status IN ('running', 'checking', 'reviewing')
                  )
                LIMIT 1
                "#,
                [&task_id],
                |row| {
                    Ok(RuntimeTaskContext {
                        task_id: row.get(0)?,
                        task_path: row.get(1)?,
                        run_dir: String::new(),
                        status: row.get(2)?,
                        paused_status: row.get(3)?,
                        check_retries: row.get::<_, i64>(4)? as u32,
                        review_cycles: row.get::<_, i64>(5)? as u32,
                        failure_reason: row.get(6)?,
                        run_id: Some(run_id.clone()),
                        workspace_path: None,
                    })
                },
            )
            .optional()?;
        let Some(mut context) = candidate else {
            transaction.commit()?;
            return Ok(None);
        };

        context.run_dir = run_dir_for_task(&context.task_id);
        let attached = transaction.execute(
            r#"
            UPDATE runs
            SET task_id = ?1, updated_at = ?2
            WHERE id = ?3
              AND NOT EXISTS (
                  SELECT 1
                  FROM runs active_runs
                  WHERE active_runs.task_id = ?1
                    AND active_runs.role = 'supervisor'
                    AND active_runs.status IN ('running', 'checking', 'reviewing')
                    AND active_runs.id <> ?3
              )
            "#,
            params![context.task_id, timestamp(), run_id],
        )?;
        if attached == 0 {
            transaction.commit()?;
            return Ok(None);
        }
        insert_event_in_transaction(
            &transaction,
            Some(&run_id),
            "run_consultation_attached",
            &serde_json::json!({
                "agent": agent_id,
                "task_id": context.task_id,
            }),
        )?;
        transaction.commit()?;
        Ok(Some(context))
    })
    .await?
}

pub async fn record_run_finished(run_id: &str, exit_code: i32) -> Result<()> {
    let database_path = current_database_path().await?;
    let run_id = run_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        let status = if exit_code == 0 {
            "completed"
        } else {
            "failed"
        };
        connection.execute(
            "UPDATE runs SET status = ?1, updated_at = ?2, pid = NULL WHERE id = ?3",
            params![status, timestamp(), run_id],
        )?;
        insert_event(
            &connection,
            Some(&run_id),
            "run_finished",
            &serde_json::json!({
                "exit_code": exit_code,
                "status": status,
            }),
        )?;
        Ok(())
    })
    .await?
}

pub async fn record_run_finished_best_effort(run_id: &str, exit_code: i32) {
    if let Err(err) = record_run_finished(run_id, exit_code).await {
        warn!(error = ?err, run_id, exit_code, "failed to mirror run finish into ferrus.db");
    }
}

pub async fn recover_interrupted_runs() -> Result<usize> {
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || -> Result<usize> {
        let connection = open_runtime_database(&database_path)?;
        let mut statement = connection.prepare(
            "SELECT id, pid FROM runs WHERE status IN ('running', 'checking', 'reviewing')",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
        })?;

        let mut interrupted = Vec::new();
        for row in rows {
            let (run_id, pid) = row?;
            if pid.is_none_or(|pid| !process_is_alive(pid as u32)) {
                interrupted.push(run_id);
            }
        }

        for run_id in &interrupted {
            connection.execute(
                "UPDATE runs SET status = 'interrupted', updated_at = ?1, pid = NULL WHERE id = ?2",
                params![timestamp(), run_id],
            )?;
            insert_event(
                &connection,
                Some(run_id),
                "run_interrupted",
                &serde_json::json!({}),
            )?;
        }

        Ok(interrupted.len())
    })
    .await?
}

pub async fn recover_expired_task_leases() -> Result<usize> {
    let database_path = current_database_path().await?;
    tokio::task::spawn_blocking(move || -> Result<usize> {
        let connection = open_runtime_database(&database_path)?;
        let now = Utc::now();
        let mut statement = connection.prepare(
            "SELECT id, claimed_by, lease_until FROM tasks WHERE claimed_by IS NOT NULL",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;

        let mut expired = Vec::new();
        for row in rows {
            let (task_id, claimed_by, lease_until) = row?;
            let parsed_lease = lease_until
                .as_deref()
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.with_timezone(&Utc));
            if parsed_lease.is_none_or(|lease_until| now >= lease_until) {
                expired.push((task_id, claimed_by, lease_until));
            }
        }

        for (task_id, claimed_by, lease_until) in &expired {
            clear_task_lease(&connection, task_id)?;
            insert_event(
                &connection,
                None,
                "task_lease_expired",
                &serde_json::json!({
                    "task_id": task_id,
                    "claimed_by": claimed_by,
                    "lease_until": lease_until,
                }),
            )?;
        }

        Ok(expired.len())
    })
    .await?
}

pub async fn recover_runtime_state() -> Result<RuntimeRecovery> {
    let interrupted_runs = recover_interrupted_runs().await?;
    let expired_task_leases = recover_expired_task_leases().await?;
    let state_lease_mirrors_cleared = recover_state_lease_mirror().await?;
    Ok(RuntimeRecovery {
        interrupted_runs,
        expired_task_leases,
        state_lease_mirrors_cleared,
    })
}

pub async fn preview_runtime_recovery() -> Result<RuntimeRecovery> {
    let database_path = current_database_path().await?;
    preview_runtime_recovery_from(&database_path).await
}

pub async fn preview_orphaned_worktrees() -> Result<usize> {
    Ok(orphaned_worktrees().await?.len())
}

pub async fn recover_orphaned_worktrees() -> Result<usize> {
    let registration = touch_current_project().await?;
    let project_root = PathBuf::from(&registration.metadata.workspace_dir);
    let worktrees = orphaned_worktrees_for(&registration).await?;
    let mut removed = 0usize;
    for worktree in worktrees {
        let output = Command::new("git")
            .arg("-C")
            .arg(&project_root)
            .args(["worktree", "remove", "--force"])
            .arg(&worktree)
            .output()
            .await
            .with_context(|| {
                format!(
                    "Failed to run git worktree remove for {}",
                    worktree.display()
                )
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            anyhow::bail!(
                "Failed to remove orphaned worktree at {}: {}",
                worktree.display(),
                if stderr.is_empty() {
                    output.status.to_string()
                } else {
                    stderr
                }
            );
        }
        removed += 1;
    }
    Ok(removed)
}

async fn orphaned_worktrees() -> Result<Vec<PathBuf>> {
    let registration = touch_current_project().await?;
    orphaned_worktrees_for(&registration).await
}

async fn orphaned_worktrees_for(registration: &ProjectRegistration) -> Result<Vec<PathBuf>> {
    let worktrees_dir = registration.data_dir.join("worktrees");
    if !tokio::fs::try_exists(&worktrees_dir).await? {
        return Ok(Vec::new());
    }

    let protected_task_ids = protected_worktree_task_ids(&registration.database_path).await?;
    let protected_paths = protected_worktree_paths(&registration.database_path).await?;
    let mut entries = tokio::fs::read_dir(&worktrees_dir)
        .await
        .with_context(|| format!("Failed to read {}", worktrees_dir.display()))?;
    let mut orphaned = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !entry.file_type().await?.is_dir() {
            continue;
        }
        let task_id = entry.file_name().to_string_lossy().to_string();
        let canonical_path = tokio::fs::canonicalize(&path)
            .await
            .unwrap_or_else(|_| path.clone());
        if protected_task_ids.contains(&task_id) || protected_paths.contains(&canonical_path) {
            continue;
        }
        orphaned.push(path);
    }
    orphaned.sort();
    Ok(orphaned)
}

async fn protected_worktree_task_ids(
    database_path: &Path,
) -> Result<std::collections::HashSet<String>> {
    let database_path = database_path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<std::collections::HashSet<String>> {
        let connection = open_runtime_database(&database_path)?;
        let mut statement = connection.prepare(
            "SELECT id FROM tasks WHERE status NOT IN ('idle', 'reset', 'complete', 'failed')",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut task_ids = std::collections::HashSet::new();
        for row in rows {
            task_ids.insert(row?);
        }
        Ok(task_ids)
    })
    .await?
}

async fn protected_worktree_paths(
    database_path: &Path,
) -> Result<std::collections::HashSet<PathBuf>> {
    let database_path = database_path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<std::collections::HashSet<PathBuf>> {
        let connection = open_runtime_database(&database_path)?;
        let mut statement = connection.prepare(
            "SELECT workspace_path, pid FROM runs WHERE status IN ('running', 'checking', 'reviewing')",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
        })?;
        let mut paths = std::collections::HashSet::new();
        for row in rows {
            let (workspace_path, pid) = row?;
            if pid.is_none_or(|pid| !process_is_alive(pid as u32)) {
                continue;
            }
            let path = PathBuf::from(workspace_path);
            paths.insert(std::fs::canonicalize(&path).unwrap_or(path));
        }
        Ok(paths)
    })
    .await?
}

async fn preview_runtime_recovery_from(database_path: &Path) -> Result<RuntimeRecovery> {
    Ok(RuntimeRecovery {
        interrupted_runs: preview_interrupted_runs(database_path).await?,
        expired_task_leases: preview_expired_task_leases(database_path).await?,
        state_lease_mirrors_cleared: preview_state_lease_mirror().await?,
    })
}

async fn preview_interrupted_runs(database_path: &Path) -> Result<usize> {
    let database_path = database_path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<usize> {
        let connection =
            Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .with_context(|| format!("Failed to open {}", database_path.display()))?;
        let mut statement = connection
            .prepare("SELECT pid FROM runs WHERE status IN ('running', 'checking', 'reviewing')")?;
        let rows = statement.query_map([], |row| row.get::<_, Option<i64>>(0))?;

        let mut interrupted = 0;
        for row in rows {
            if row?.is_none_or(|pid| !process_is_alive(pid as u32)) {
                interrupted += 1;
            }
        }
        Ok(interrupted)
    })
    .await?
}

async fn preview_expired_task_leases(database_path: &Path) -> Result<usize> {
    let database_path = database_path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<usize> {
        let connection =
            Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .with_context(|| format!("Failed to open {}", database_path.display()))?;
        let now = Utc::now();
        let mut statement =
            connection.prepare("SELECT lease_until FROM tasks WHERE claimed_by IS NOT NULL")?;
        let rows = statement.query_map([], |row| row.get::<_, Option<String>>(0))?;

        let mut expired = 0;
        for row in rows {
            let parsed_lease = row?
                .as_deref()
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.with_timezone(&Utc));
            if parsed_lease.is_none_or(|lease_until| now >= lease_until) {
                expired += 1;
            }
        }
        Ok(expired)
    })
    .await?
}

async fn preview_state_lease_mirror() -> Result<usize> {
    let Ok(state) = crate::state::store::read_state().await else {
        return Ok(0);
    };
    if state.claimed_by.is_none() {
        return Ok(0);
    }
    if state
        .lease_until
        .is_some_and(|lease_until| Utc::now() < lease_until)
    {
        return Ok(0);
    }
    Ok(1)
}

async fn recover_state_lease_mirror() -> Result<usize> {
    let Ok(mut state) = crate::state::store::read_state().await else {
        return Ok(0);
    };
    if state.claimed_by.is_none() {
        return Ok(0);
    }
    if state
        .lease_until
        .is_some_and(|lease_until| Utc::now() < lease_until)
    {
        return Ok(0);
    }

    let task_id = state.active_task_id.clone();
    let claimed_by = state.claimed_by.clone();
    let lease_until = state.lease_until;
    state.clear_lease();
    crate::state::store::write_state(&state).await?;
    record_runtime_event(
        None,
        "state_lease_mirror_cleared",
        serde_json::json!({
            "task_id": task_id,
            "claimed_by": claimed_by,
            "lease_until": lease_until,
        }),
    )
    .await?;
    Ok(1)
}

pub fn task_status_for_state(state: &TaskState) -> &'static str {
    match state {
        TaskState::Idle => "idle",
        TaskState::Executing => "executing",
        TaskState::Consultation => "consultation",
        TaskState::Reviewing => "reviewing",
        TaskState::Addressing => "addressing",
        TaskState::Complete => "complete",
        TaskState::Failed => "failed",
        TaskState::AwaitingHuman => "awaiting_human",
    }
}

async fn initialize_database(path: &Path) -> Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = Connection::open(&path)
            .with_context(|| format!("Failed to open {}", path.display()))?;
        initialize_schema(&connection)?;
        Ok(())
    })
    .await?
}

async fn validate_database_schema(path: &Path) -> Result<bool> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<bool> {
        let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("Failed to open {}", path.display()))?;
        for table in ["tasks", "runs", "events"] {
            let exists: i64 = connection.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )?;
            if exists == 0 {
                return Ok(false);
            }
        }
        for column in [
            "paused_status",
            "claimed_by",
            "lease_until",
            "last_heartbeat",
            "check_retries",
            "review_cycles",
            "failure_reason",
        ] {
            if !column_exists(&connection, "tasks", column)? {
                return Ok(false);
            }
        }
        Ok(true)
    })
    .await?
}

async fn max_task_number_from_files(tasks_dir: &Path) -> Result<u32> {
    let mut max_number = 0;
    let mut entries = tokio::fs::read_dir(tasks_dir)
        .await
        .with_context(|| format!("Failed to read {}", tasks_dir.display()))?;
    while let Some(entry) = entries.next_entry().await? {
        let Some(file_name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if let Some(number) = parse_task_number(file_name.strip_suffix(".md").unwrap_or(&file_name))
        {
            max_number = max_number.max(number);
        }
    }
    Ok(max_number)
}

async fn max_task_number_from_database(path: &Path) -> Result<u32> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<u32> {
        let connection = open_runtime_database(&path)?;
        let mut statement = connection.prepare("SELECT id FROM tasks WHERE id LIKE 't-%'")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut max_number = 0;
        for row in rows {
            if let Some(number) = parse_task_number(&row?) {
                max_number = max_number.max(number);
            }
        }
        Ok(max_number)
    })
    .await?
}

async fn read_task_record_from_database(path: &Path, task_id: &str) -> Result<Option<TaskRecord>> {
    let path = path.to_path_buf();
    let task_id = task_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<Option<TaskRecord>> {
        let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("Failed to open {}", path.display()))?;
        let task = connection
            .query_row(
                r#"
                SELECT id, path, spec_path, milestone_id, status, paused_status, claimed_by,
                       lease_until, last_heartbeat, check_retries, review_cycles, failure_reason
                FROM tasks
                WHERE id = ?1
                "#,
                [task_id],
                task_record_from_row,
            )
            .optional()?;
        Ok(task)
    })
    .await?
}

async fn current_database_path() -> Result<PathBuf> {
    let local_ref = read_local_project_ref()
        .await
        .context(".ferrus/project.toml not found — run `ferrus migrate`")?;
    Ok(PathBuf::from(local_ref.data_dir).join("ferrus.db"))
}

async fn current_task_record() -> CurrentTaskRecord {
    let Ok(state) = crate::state::store::read_state().await else {
        return CurrentTaskRecord {
            id: CURRENT_TASK_ID.to_string(),
            path: CURRENT_TASK_PATH.to_string(),
            spec_path: None,
            milestone_id: None,
        };
    };
    CurrentTaskRecord {
        id: state
            .active_task_id
            .unwrap_or_else(|| CURRENT_TASK_ID.to_string()),
        path: state
            .active_task_path
            .unwrap_or_else(|| CURRENT_TASK_PATH.to_string()),
        spec_path: state.task_spec,
        milestone_id: state.task_milestone,
    }
}

async fn current_task_identity() -> (String, String) {
    let task = current_task_record().await;
    (task.id, task.path)
}

fn open_runtime_database(path: &Path) -> Result<Connection> {
    let connection =
        Connection::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    initialize_schema(&connection)?;
    Ok(connection)
}

fn initialize_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        r#"
        PRAGMA foreign_keys = ON;

        CREATE TABLE IF NOT EXISTS tasks (
            id TEXT PRIMARY KEY,
            path TEXT NOT NULL,
            status TEXT NOT NULL,
            paused_status TEXT,
            spec_path TEXT,
            milestone_id TEXT,
            claimed_by TEXT,
            lease_until TEXT,
            last_heartbeat TEXT,
            check_retries INTEGER NOT NULL DEFAULT 0,
            review_cycles INTEGER NOT NULL DEFAULT 0,
            failure_reason TEXT
        );

        CREATE TABLE IF NOT EXISTS runs (
            id TEXT PRIMARY KEY,
            task_id TEXT NOT NULL,
            role TEXT NOT NULL,
            agent TEXT NOT NULL,
            status TEXT NOT NULL,
            started_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            pid INTEGER,
            workspace_path TEXT NOT NULL,
            FOREIGN KEY(task_id) REFERENCES tasks(id)
        );

        CREATE TABLE IF NOT EXISTS events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            run_id TEXT,
            type TEXT NOT NULL,
            payload_json TEXT NOT NULL,
            created_at TEXT NOT NULL,
            FOREIGN KEY(run_id) REFERENCES runs(id)
        );
        "#,
    )?;
    ensure_column(connection, "tasks", "paused_status", "TEXT")?;
    ensure_column(connection, "tasks", "spec_path", "TEXT")?;
    ensure_column(connection, "tasks", "milestone_id", "TEXT")?;
    ensure_column(connection, "tasks", "claimed_by", "TEXT")?;
    ensure_column(connection, "tasks", "lease_until", "TEXT")?;
    ensure_column(connection, "tasks", "last_heartbeat", "TEXT")?;
    ensure_column(
        connection,
        "tasks",
        "check_retries",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(
        connection,
        "tasks",
        "review_cycles",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(connection, "tasks", "failure_reason", "TEXT")?;
    Ok(())
}

fn upsert_task(
    connection: &Connection,
    id: &str,
    path: &str,
    status: &str,
    spec_path: Option<&str>,
    milestone_id: Option<&str>,
) -> Result<()> {
    connection.execute(
        r#"
        INSERT INTO tasks (id, path, status, spec_path, milestone_id)
        VALUES (?1, ?2, ?3, ?4, ?5)
        ON CONFLICT(id) DO UPDATE SET
            path = excluded.path,
            status = excluded.status,
            spec_path = COALESCE(excluded.spec_path, tasks.spec_path),
            milestone_id = COALESCE(excluded.milestone_id, tasks.milestone_id)
        "#,
        params![id, path, status, spec_path, milestone_id],
    )?;
    Ok(())
}

fn ensure_task_exists(connection: &Connection, id: &str, path: &str) -> Result<()> {
    connection.execute(
        "INSERT OR IGNORE INTO tasks (id, path, status) VALUES (?1, ?2, 'unknown')",
        params![id, path],
    )?;
    Ok(())
}

fn task_check_retries(connection: &Connection, task_id: &str) -> Result<u32> {
    let retries = connection
        .query_row(
            "SELECT check_retries FROM tasks WHERE id = ?1",
            [task_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0);
    Ok(retries as u32)
}

fn task_review_cycles(connection: &Connection, task_id: &str) -> Result<u32> {
    let cycles = connection
        .query_row(
            "SELECT review_cycles FROM tasks WHERE id = ?1",
            [task_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0);
    Ok(cycles as u32)
}

struct ReadyTaskCandidate {
    id: String,
    path: String,
    status: String,
    paused_status: Option<String>,
    check_retries: u32,
    review_cycles: u32,
    failure_reason: Option<String>,
    claimed_by: Option<String>,
    lease_until: Option<String>,
}

fn task_candidates_by_status(
    transaction: &Transaction<'_>,
    statuses: &[String],
) -> Result<Vec<ReadyTaskCandidate>> {
    if statuses.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = std::iter::repeat_n("?", statuses.len())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        r#"
        SELECT id, path, status, paused_status, check_retries, review_cycles, failure_reason,
               claimed_by, lease_until
        FROM tasks
        WHERE status IN ({placeholders})
        ORDER BY id
        "#
    );
    let mut statement = transaction.prepare(&sql)?;
    let rows = statement.query_map(rusqlite::params_from_iter(statuses.iter()), |row| {
        Ok(ReadyTaskCandidate {
            id: row.get(0)?,
            path: row.get(1)?,
            status: row.get(2)?,
            paused_status: row.get(3)?,
            check_retries: row.get::<_, i64>(4)? as u32,
            review_cycles: row.get::<_, i64>(5)? as u32,
            failure_reason: row.get(6)?,
            claimed_by: row.get(7)?,
            lease_until: row.get(8)?,
        })
    })?;

    let mut tasks = Vec::new();
    for row in rows {
        tasks.push(row?);
    }
    Ok(tasks)
}

fn task_candidate_by_id(
    transaction: &Transaction<'_>,
    task_id: &str,
) -> Result<Option<ReadyTaskCandidate>> {
    let task = transaction
        .query_row(
            r#"
            SELECT id, path, status, paused_status, check_retries, review_cycles, failure_reason,
                   claimed_by, lease_until
            FROM tasks
            WHERE id = ?1
            LIMIT 1
            "#,
            [task_id],
            |row| {
                Ok(ReadyTaskCandidate {
                    id: row.get(0)?,
                    path: row.get(1)?,
                    status: row.get(2)?,
                    paused_status: row.get(3)?,
                    check_retries: row.get::<_, i64>(4)? as u32,
                    review_cycles: row.get::<_, i64>(5)? as u32,
                    failure_reason: row.get(6)?,
                    claimed_by: row.get(7)?,
                    lease_until: row.get(8)?,
                })
            },
        )
        .optional()?;
    Ok(task)
}

fn claim_task_in_transaction(
    transaction: &Transaction<'_>,
    task_id: &str,
    agent_id: &str,
    lease_until: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<()> {
    let lease_until_text = lease_until.to_rfc3339_opts(SecondsFormat::Secs, true);
    let now_text = now.to_rfc3339_opts(SecondsFormat::Secs, true);
    transaction.execute(
        "UPDATE tasks SET claimed_by = ?1, lease_until = ?2, last_heartbeat = ?3 WHERE id = ?4",
        params![agent_id, lease_until_text, now_text, task_id],
    )?;
    insert_event_in_transaction(
        transaction,
        None,
        "task_claimed",
        &serde_json::json!({
            "task_id": task_id,
            "claimed_by": agent_id,
            "lease_until": lease_until,
        }),
    )?;
    Ok(())
}

fn parse_lease_until(value: Option<&str>) -> Option<DateTime<Utc>> {
    value
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

fn clear_task_lease(connection: &Connection, task_id: &str) -> Result<()> {
    connection.execute(
        "UPDATE tasks SET claimed_by = NULL, lease_until = NULL, last_heartbeat = NULL WHERE id = ?1",
        [task_id],
    )?;
    Ok(())
}

fn insert_event(
    connection: &Connection,
    run_id: Option<&str>,
    event_type: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    if let Some(run_id) = run_id {
        let exists = connection
            .query_row("SELECT 1 FROM runs WHERE id = ?1", [run_id], |_| Ok(()))
            .optional()?
            .is_some();
        if !exists {
            anyhow::bail!("Cannot insert event for unknown run id {run_id}");
        }
    }
    connection.execute(
        "INSERT INTO events (run_id, type, payload_json, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![
            run_id,
            event_type,
            serde_json::to_string(payload)?,
            timestamp()
        ],
    )?;
    Ok(())
}

fn insert_event_in_transaction(
    transaction: &Transaction<'_>,
    run_id: Option<&str>,
    event_type: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    transaction.execute(
        "INSERT INTO events (run_id, type, payload_json, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![
            run_id,
            event_type,
            serde_json::to_string(payload)?,
            timestamp()
        ],
    )?;
    Ok(())
}

fn ensure_column(
    connection: &Connection,
    table_name: &str,
    column_name: &str,
    column_type: &str,
) -> Result<()> {
    if column_exists(connection, table_name, column_name)? {
        return Ok(());
    }
    connection.execute(
        &format!("ALTER TABLE {table_name} ADD COLUMN {column_name} {column_type}"),
        [],
    )?;
    Ok(())
}

fn column_exists(connection: &Connection, table_name: &str, column_name: &str) -> Result<bool> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table_name})"))?;
    let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
    for column in columns {
        if column? == column_name {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn copy_legacy_artifacts() -> Result<()> {
    copy_if_nonempty(".ferrus/TASK.md", ".ferrus/tasks/t-001.md").await?;
    tokio::fs::create_dir_all(".ferrus/runs/t-001")
        .await
        .context("Failed to create .ferrus/runs/t-001")?;
    copy_if_nonempty(".ferrus/REVIEW.md", ".ferrus/runs/t-001/REVIEW.md").await?;
    copy_if_nonempty(".ferrus/SUBMISSION.md", ".ferrus/runs/t-001/SUBMISSION.md").await?;
    Ok(())
}

fn populate_legacy_active_artifacts(state: &mut crate::state::machine::StateData) -> bool {
    if state.state == TaskState::Idle || state.active_task_id.is_some() {
        return false;
    }
    state.set_active_task_artifacts(
        "t-001".to_string(),
        ".ferrus/tasks/t-001.md".to_string(),
        ".ferrus/runs/t-001".to_string(),
    );
    true
}

async fn copy_if_nonempty(from: &str, to: &str) -> Result<()> {
    if Path::new(to).exists() {
        return Ok(());
    }
    let Ok(contents) = tokio::fs::read_to_string(from).await else {
        return Ok(());
    };
    if contents.trim().is_empty() {
        return Ok(());
    }
    tokio::fs::write(to, contents)
        .await
        .with_context(|| format!("Failed to write {to}"))
}

async fn read_local_project_ref() -> Result<LocalProjectRef> {
    let path = project_path(LOCAL_PROJECT_TOML);
    let contents = tokio::fs::read_to_string(&path)
        .await
        .context("Failed to read .ferrus/project.toml")?;
    toml::from_str(&contents).context("Failed to parse .ferrus/project.toml")
}

fn project_path(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    if path.is_absolute() || !starts_with_ferrus_dir(path) {
        return path.to_path_buf();
    }
    std::env::var(ENV_PROJECT_ROOT)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|root| root.join(path))
        .unwrap_or_else(|| path.to_path_buf())
}

fn starts_with_ferrus_dir(path: &Path) -> bool {
    path.components()
        .next()
        .and_then(|component| match component {
            std::path::Component::Normal(value) => value.to_str(),
            _ => None,
        })
        == Some(".ferrus")
}

async fn read_project_metadata_from(path: &Path) -> Result<ProjectMetadata> {
    let contents = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("Failed to read {}", path.display()))?;
    toml::from_str(&contents).with_context(|| format!("Failed to parse {}", path.display()))
}

async fn write_toml<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let contents = toml::to_string_pretty(value).context("Failed to serialize project metadata")?;
    tokio::fs::write(path, contents)
        .await
        .with_context(|| format!("Failed to write {}", path.display()))
}

fn global_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Cannot determine home directory")?;
    Ok(home.join(".ferrus"))
}

fn project_data_dir(project_id: &str) -> Result<PathBuf> {
    validate_project_id(project_id)?;
    Ok(global_dir()?.join("projects").join(project_id))
}

async fn canonical_current_dir() -> Result<PathBuf> {
    let current = std::env::current_dir().context("Failed to read current directory")?;
    tokio::fs::canonicalize(current)
        .await
        .context("Failed to canonicalize current directory")
}

async fn equivalent_paths(left: &Path, right: &Path) -> bool {
    let left = tokio::fs::canonicalize(left)
        .await
        .unwrap_or_else(|_| left.to_path_buf());
    let right = tokio::fs::canonicalize(right)
        .await
        .unwrap_or_else(|_| right.to_path_buf());
    left == right
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn generate_project_id(workspace_dir: &Path) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let mut hasher = DefaultHasher::new();
    workspace_dir.hash(&mut hasher);
    millis.hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    let hash = hasher.finish();
    format!("P{:012X}{:016X}", millis & 0xFFFFFFFFFFFF, hash)
}

fn generate_run_id(role: &str, agent: &str) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let mut hasher = DefaultHasher::new();
    role.hash(&mut hasher);
    agent.hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    RUN_ID_COUNTER
        .fetch_add(1, Ordering::Relaxed)
        .hash(&mut hasher);
    millis.hash(&mut hasher);
    let hash = hasher.finish();
    format!("r-{:012x}-{:016x}", millis & 0xFFFFFFFFFFFF, hash)
}

fn parse_task_number(task_id: &str) -> Option<u32> {
    task_id.strip_prefix("t-")?.parse().ok()
}

fn validate_project_id(project_id: &str) -> Result<()> {
    let valid = !project_id.is_empty()
        && project_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if valid {
        Ok(())
    } else {
        anyhow::bail!("Invalid project_id in .ferrus/project.toml: {project_id:?}")
    }
}

#[derive(Debug)]
struct GitMetadata {
    origin_repo: Option<String>,
    default_branch: Option<String>,
    current_head: Option<String>,
}

async fn read_git_metadata() -> Option<GitMetadata> {
    if git_output(["rev-parse", "--is-inside-work-tree"]).await? != "true" {
        return None;
    }
    Some(GitMetadata {
        origin_repo: git_output(["config", "--get", "remote.origin.url"]).await,
        default_branch: read_default_branch().await,
        current_head: git_output(["rev-parse", "HEAD"]).await,
    })
}

async fn read_default_branch() -> Option<String> {
    if let Some(branch) = git_output(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]).await
    {
        return branch
            .strip_prefix("origin/")
            .unwrap_or(&branch)
            .to_string()
            .into();
    }
    git_output(["rev-parse", "--abbrev-ref", "HEAD"]).await
}

async fn git_output<const N: usize>(args: [&str; N]) -> Option<String> {
    let output = Command::new("git").args(args).output().await.ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn process_is_alive(pid: u32) -> bool {
    platform::pid_is_alive(pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{machine::StateData, store};
    use tempfile::TempDir;

    async fn setup_project() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        let workspace = dir.path();
        let data_dir = workspace.join(".ferrus/projects/test-project");
        std::fs::create_dir_all(workspace.join(".ferrus")).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();
        write_toml(
            &workspace.join(".ferrus/project.toml"),
            &LocalProjectRef {
                project_id: "test-project".to_string(),
                name: "test".to_string(),
                data_dir: path_string(&data_dir),
            },
        )
        .await
        .unwrap();
        std::env::set_current_dir(workspace).unwrap();
        initialize_database(&data_dir.join("ferrus.db"))
            .await
            .unwrap();

        let mut state = StateData::default();
        state.state = TaskState::Executing;
        state.set_active_task_artifacts(
            "t-001".to_string(),
            ".ferrus/tasks/t-001.md".to_string(),
            ".ferrus/runs/t-001".to_string(),
        );
        store::write_state(&state).await.unwrap();
        record_task_status("t-001", ".ferrus/tasks/t-001.md", "executing")
            .await
            .unwrap();

        (dir, previous)
    }

    fn teardown(previous: PathBuf) {
        std::env::set_current_dir(previous).unwrap();
    }

    #[test]
    fn legacy_non_idle_state_gets_default_active_artifacts_for_migration() {
        let mut state = StateData {
            state: TaskState::Executing,
            ..StateData::default()
        };

        assert!(populate_legacy_active_artifacts(&mut state));
        assert_eq!(state.active_task_id.as_deref(), Some("t-001"));
        assert_eq!(
            state.active_task_path.as_deref(),
            Some(".ferrus/tasks/t-001.md")
        );
        assert_eq!(state.active_run_dir.as_deref(), Some(".ferrus/runs/t-001"));
    }

    #[test]
    fn legacy_artifact_population_leaves_idle_and_existing_artifacts_unchanged() {
        let mut idle = StateData::default();
        assert!(!populate_legacy_active_artifacts(&mut idle));
        assert!(idle.active_task_id.is_none());

        let mut migrated = StateData {
            state: TaskState::Addressing,
            ..StateData::default()
        };
        migrated.set_active_task_artifacts(
            "t-009".to_string(),
            ".ferrus/tasks/t-009.md".to_string(),
            ".ferrus/runs/t-009".to_string(),
        );

        assert!(!populate_legacy_active_artifacts(&mut migrated));
        assert_eq!(migrated.active_task_id.as_deref(), Some("t-009"));
        assert_eq!(
            migrated.active_task_path.as_deref(),
            Some(".ferrus/tasks/t-009.md")
        );
        assert_eq!(
            migrated.active_run_dir.as_deref(),
            Some(".ferrus/runs/t-009")
        );
    }

    #[tokio::test]
    async fn sqlite_task_claim_is_exclusive_and_renewable() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;

        let first = claim_current_task("executor:codex:1", 60).await.unwrap();
        assert!(matches!(first, TaskClaim::Claimed { .. }));

        let second = claim_current_task("executor:codex:1", 60).await.unwrap();
        assert!(matches!(second, TaskClaim::AlreadyClaimed { .. }));

        let other = claim_current_task("executor:codex:2", 60).await.unwrap();
        match other {
            TaskClaim::ClaimedByOther { claimed_by } => {
                assert_eq!(claimed_by, "executor:codex:1");
            }
            _ => panic!("expected claimed_by_other"),
        }

        let renewed = renew_current_task_lease("executor:codex:1", 60)
            .await
            .unwrap();
        assert!(matches!(renewed, LeaseRenewal::Renewed { .. }));

        teardown(previous);
    }

    #[tokio::test]
    async fn sqlite_task_claim_can_target_non_current_task() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;

        let first = claim_task("t-002", ".ferrus/tasks/t-002.md", "executor:codex:2", 60)
            .await
            .unwrap();
        assert!(matches!(first, TaskClaim::Claimed { .. }));

        let second = claim_task("t-002", ".ferrus/tasks/t-002.md", "executor:codex:3", 60)
            .await
            .unwrap();
        match second {
            TaskClaim::ClaimedByOther { claimed_by } => {
                assert_eq!(claimed_by, "executor:codex:2");
            }
            _ => panic!("expected claimed_by_other"),
        }

        let tasks = list_tasks().await.unwrap();
        let current = tasks.iter().find(|task| task.id == "t-001").unwrap();
        let targeted = tasks.iter().find(|task| task.id == "t-002").unwrap();
        assert_eq!(current.claimed_by, None);
        assert_eq!(targeted.path, ".ferrus/tasks/t-002.md");
        assert_eq!(targeted.status, "unknown");
        assert_eq!(targeted.claimed_by.as_deref(), Some("executor:codex:2"));

        teardown(previous);
    }

    #[tokio::test]
    async fn sqlite_task_lease_can_be_renewed_by_claiming_agent() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;

        let first = claim_task("t-002", ".ferrus/tasks/t-002.md", "executor:codex:2", 60)
            .await
            .unwrap();
        assert!(matches!(first, TaskClaim::Claimed { .. }));

        store::write_state(&StateData::default()).await.unwrap();

        let renewed = renew_claimed_task_lease("executor:codex:2", 60)
            .await
            .unwrap();
        match renewed {
            LeaseRenewal::Renewed {
                task_id,
                task_path,
                claimed_by,
                ..
            } => {
                assert_eq!(task_id, "t-002");
                assert_eq!(task_path, ".ferrus/tasks/t-002.md");
                assert_eq!(claimed_by, "executor:codex:2");
            }
            _ => panic!("expected claimed task lease to renew"),
        }

        let missing = renew_claimed_task_lease("executor:codex:3", 60)
            .await
            .unwrap();
        assert!(matches!(missing, LeaseRenewal::NotClaimed));

        teardown(previous);
    }

    #[tokio::test]
    async fn runtime_task_context_resolves_claimed_task_by_agent() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;

        record_task_status("t-002", ".ferrus/tasks/t-002.md", "executing")
            .await
            .unwrap();
        claim_task("t-002", ".ferrus/tasks/t-002.md", "executor:codex:2", 60)
            .await
            .unwrap();

        let context = runtime_task_context_for_agent("executor:codex:2")
            .await
            .unwrap()
            .unwrap();

        assert_eq!(context.task_id, "t-002");
        assert_eq!(context.task_path, ".ferrus/tasks/t-002.md");
        assert_eq!(context.run_dir, ".ferrus/runs/t-002");
        assert_eq!(context.status, "executing");
        assert!(context.run_id.is_none());

        teardown(previous);
    }

    #[tokio::test]
    async fn sqlite_claim_next_ready_task_skips_active_claims_and_preserves_agent_lease() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        record_task_status("t-002", ".ferrus/tasks/t-002.md", "executing")
            .await
            .unwrap();
        record_task_status("t-003", ".ferrus/tasks/t-003.md", "reviewing")
            .await
            .unwrap();

        let first = claim_next_ready_task("executor:codex:1", 60).await.unwrap();
        match first {
            ReadyTaskClaim::Claimed(task) => {
                assert_eq!(task.task_id, "t-001");
                assert_eq!(task.task_path, ".ferrus/tasks/t-001.md");
                assert_eq!(task.status, "executing");
                assert_eq!(task.claimed_by, "executor:codex:1");
            }
            _ => panic!("expected first ready task to be claimed"),
        }

        let same_agent = claim_next_ready_task("executor:codex:1", 60).await.unwrap();
        match same_agent {
            ReadyTaskClaim::AlreadyClaimed(task) => {
                assert_eq!(task.task_id, "t-001");
                assert_eq!(task.claimed_by, "executor:codex:1");
            }
            _ => panic!("expected existing agent lease"),
        }

        let other_agent = claim_next_ready_task("executor:codex:2", 60).await.unwrap();
        match other_agent {
            ReadyTaskClaim::Claimed(task) => {
                assert_eq!(task.task_id, "t-002");
                assert_eq!(task.task_path, ".ferrus/tasks/t-002.md");
                assert_eq!(task.status, "executing");
                assert_eq!(task.claimed_by, "executor:codex:2");
            }
            _ => panic!("expected second ready task to be claimed"),
        }

        let no_available = claim_next_ready_task("executor:codex:3", 60).await.unwrap();
        assert!(matches!(no_available, ReadyTaskClaim::NoAvailable));

        let tasks = list_tasks().await.unwrap();
        let reviewing = tasks.iter().find(|task| task.id == "t-003").unwrap();
        assert_eq!(reviewing.claimed_by, None);

        teardown(previous);
    }

    #[tokio::test]
    async fn sqlite_claim_ready_task_by_id_promotes_pending_task() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        record_task_status("t-002", ".ferrus/tasks/t-002.md", "pending")
            .await
            .unwrap();

        let claim = claim_ready_task_by_id("t-002", "executor:codex:t-002", 60)
            .await
            .unwrap();

        match claim {
            ReadyTaskClaim::Claimed(task) => {
                assert_eq!(task.task_id, "t-002");
                assert_eq!(task.task_path, ".ferrus/tasks/t-002.md");
                assert_eq!(task.status, "executing");
                assert_eq!(task.claimed_by, "executor:codex:t-002");
            }
            _ => panic!("expected pending task to be promoted and claimed"),
        }

        let tasks = list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-002").unwrap();
        assert_eq!(task.status, "executing");
        assert_eq!(task.claimed_by.as_deref(), Some("executor:codex:t-002"));

        teardown(previous);
    }

    #[tokio::test]
    async fn sqlite_claim_next_review_task_claims_reviewing_rows_only() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        record_task_status("t-002", ".ferrus/tasks/t-002.md", "executing")
            .await
            .unwrap();
        record_task_status("t-003", ".ferrus/tasks/t-003.md", "reviewing")
            .await
            .unwrap();

        let claim = claim_next_review_task("supervisor:codex:1", 60)
            .await
            .unwrap();

        match claim {
            ReadyTaskClaim::Claimed(task) => {
                assert_eq!(task.task_id, "t-003");
                assert_eq!(task.task_path, ".ferrus/tasks/t-003.md");
                assert_eq!(task.status, "reviewing");
                assert_eq!(task.claimed_by, "supervisor:codex:1");
            }
            _ => panic!("expected reviewing task to be claimed"),
        }

        let tasks = list_tasks().await.unwrap();
        let executing = tasks.iter().find(|task| task.id == "t-002").unwrap();
        let reviewing = tasks.iter().find(|task| task.id == "t-003").unwrap();
        assert_eq!(executing.claimed_by, None);
        assert_eq!(reviewing.claimed_by.as_deref(), Some("supervisor:codex:1"));

        teardown(previous);
    }

    #[tokio::test]
    async fn sqlite_claim_review_task_by_id_does_not_steal_another_review() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        record_task_status("t-002", ".ferrus/tasks/t-002.md", "reviewing")
            .await
            .unwrap();
        record_task_status("t-003", ".ferrus/tasks/t-003.md", "reviewing")
            .await
            .unwrap();

        let missing = claim_review_task_by_id("t-999", "supervisor:codex:t-999", 60)
            .await
            .unwrap();
        assert!(matches!(missing, ReadyTaskClaim::NoAvailable));

        let claim = claim_review_task_by_id("t-003", "supervisor:codex:t-003", 60)
            .await
            .unwrap();
        match claim {
            ReadyTaskClaim::Claimed(task) => {
                assert_eq!(task.task_id, "t-003");
                assert_eq!(task.task_path, ".ferrus/tasks/t-003.md");
                assert_eq!(task.status, "reviewing");
                assert_eq!(task.claimed_by, "supervisor:codex:t-003");
            }
            _ => panic!("expected targeted reviewing task to be claimed"),
        }

        let tasks = list_tasks().await.unwrap();
        let other = tasks.iter().find(|task| task.id == "t-002").unwrap();
        let targeted = tasks.iter().find(|task| task.id == "t-003").unwrap();
        assert_eq!(other.claimed_by, None);
        assert_eq!(
            targeted.claimed_by.as_deref(),
            Some("supervisor:codex:t-003")
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn list_human_questions_reads_scoped_awaiting_human_tasks() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        record_task_status("t-002", ".ferrus/tasks/t-002.md", "executing")
            .await
            .unwrap();
        record_task_human_question_requested("t-002", "executing")
            .await
            .unwrap();
        crate::state::store::write_question_for_run_dir(
            ".ferrus/runs/t-002",
            "Which option should I use?",
        )
        .await
        .unwrap();

        let questions = list_human_questions().await.unwrap();

        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].task_id, "t-002");
        assert_eq!(questions[0].task_path, ".ferrus/tasks/t-002.md");
        assert_eq!(questions[0].run_dir, ".ferrus/runs/t-002");
        assert_eq!(questions[0].question, "Which option should I use?");

        teardown(previous);
    }

    #[tokio::test]
    async fn list_tasks_reads_runtime_rows() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        claim_current_task("executor:codex:1", 60).await.unwrap();

        let tasks = list_tasks().await.unwrap();

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "t-001");
        assert_eq!(tasks[0].path, ".ferrus/tasks/t-001.md");
        assert_eq!(tasks[0].status, "executing");
        assert_eq!(tasks[0].claimed_by.as_deref(), Some("executor:codex:1"));
        assert!(tasks[0].lease_until.is_some());

        teardown(previous);
    }

    #[tokio::test]
    async fn current_task_status_records_origin_metadata() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        let mut state = store::read_state().await.unwrap();
        state.task_spec = Some("docs/specs/spec.md".to_string());
        state.task_milestone = Some("m1.0".to_string());
        store::write_state(&state).await.unwrap();

        record_current_task_status("executing").await.unwrap();

        let tasks = list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-001").unwrap();
        assert_eq!(task.spec_path.as_deref(), Some("docs/specs/spec.md"));
        assert_eq!(task.milestone_id.as_deref(), Some("m1.0"));

        teardown(previous);
    }

    #[tokio::test]
    async fn task_status_update_preserves_existing_origin_metadata() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        record_task_status_with_origin(
            "t-001",
            ".ferrus/tasks/t-001.md",
            "executing",
            Some("docs/specs/spec.md"),
            Some("m1.0"),
        )
        .await
        .unwrap();

        record_task_status("t-001", ".ferrus/tasks/t-001.md", "reviewing")
            .await
            .unwrap();

        let tasks = list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-001").unwrap();
        assert_eq!(task.status, "reviewing");
        assert_eq!(task.spec_path.as_deref(), Some("docs/specs/spec.md"));
        assert_eq!(task.milestone_id.as_deref(), Some("m1.0"));

        teardown(previous);
    }

    #[tokio::test]
    async fn finds_non_terminal_task_by_origin() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        record_task_status_with_origin(
            "t-002",
            ".ferrus/tasks/t-002.md",
            "pending",
            Some("docs/specs/spec.md"),
            Some("m1.1"),
        )
        .await
        .unwrap();

        let task = find_non_terminal_task_by_origin("docs/specs/spec.md", "m1.1")
            .await
            .unwrap()
            .unwrap();

        assert_eq!(task.id, "t-002");

        record_task_status("t-002", ".ferrus/tasks/t-002.md", "complete")
            .await
            .unwrap();
        let task = find_non_terminal_task_by_origin("docs/specs/spec.md", "m1.1")
            .await
            .unwrap();
        assert!(task.is_none());

        teardown(previous);
    }

    #[tokio::test]
    async fn sqlite_task_check_failures_use_per_task_retry_budget() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        claim_current_task("executor:codex:1", 60).await.unwrap();

        let first = record_task_check_failed("t-001", "fmt failed", 2)
            .await
            .unwrap();
        assert!(matches!(first, TaskCheckFailure::Failed { retries: 1 }));

        let tasks = list_tasks().await.unwrap();
        assert_eq!(tasks[0].status, "executing");
        assert_eq!(tasks[0].check_retries, 1);
        assert_eq!(tasks[0].failure_reason.as_deref(), Some("fmt failed"));
        assert_eq!(tasks[0].claimed_by.as_deref(), Some("executor:codex:1"));

        let second = record_task_check_failed("t-001", "tests failed", 2)
            .await
            .unwrap();
        assert!(matches!(
            second,
            TaskCheckFailure::LimitExceeded { retries: 2 }
        ));

        let tasks = list_tasks().await.unwrap();
        assert_eq!(tasks[0].status, "failed");
        assert_eq!(tasks[0].check_retries, 2);
        assert_eq!(tasks[0].claimed_by, None);
        assert_eq!(tasks[0].lease_until, None);
        assert!(
            tasks[0]
                .failure_reason
                .as_deref()
                .unwrap_or_default()
                .contains("Last failure:\ntests failed")
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn sqlite_task_review_rejections_use_per_task_cycle_budget() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        record_task_status("t-001", ".ferrus/tasks/t-001.md", "reviewing")
            .await
            .unwrap();
        claim_task("t-001", ".ferrus/tasks/t-001.md", "supervisor:codex:1", 60)
            .await
            .unwrap();

        let first = record_task_review_rejected("t-001", 2).await.unwrap();
        assert!(matches!(
            first,
            TaskReviewRejection::Addressing { cycles: 1 }
        ));

        let tasks = list_tasks().await.unwrap();
        assert_eq!(tasks[0].status, "addressing");
        assert_eq!(tasks[0].review_cycles, 1);
        assert_eq!(tasks[0].check_retries, 0);
        assert_eq!(tasks[0].claimed_by, None);

        record_task_status("t-001", ".ferrus/tasks/t-001.md", "reviewing")
            .await
            .unwrap();
        let second = record_task_review_rejected("t-001", 2).await.unwrap();
        assert!(matches!(
            second,
            TaskReviewRejection::LimitExceeded { cycles: 2 }
        ));

        let tasks = list_tasks().await.unwrap();
        assert_eq!(tasks[0].status, "failed");
        assert_eq!(tasks[0].review_cycles, 2);
        assert_eq!(tasks[0].claimed_by, None);
        assert!(
            tasks[0]
                .failure_reason
                .as_deref()
                .unwrap_or_default()
                .contains("Task rejected 2 times")
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn handoff_task_statuses_clear_database_lease() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        claim_current_task("executor:codex:1", 60).await.unwrap();

        record_task_status("t-001", ".ferrus/tasks/t-001.md", "reviewing")
            .await
            .unwrap();
        let tasks = list_tasks().await.unwrap();
        assert_eq!(tasks[0].status, "reviewing");
        assert_eq!(tasks[0].claimed_by, None);
        assert_eq!(tasks[0].lease_until, None);
        assert_eq!(tasks[0].last_heartbeat, None);

        claim_current_task("executor:codex:2", 60).await.unwrap();
        record_task_status("t-001", ".ferrus/tasks/t-001.md", "addressing")
            .await
            .unwrap();
        let tasks = list_tasks().await.unwrap();
        assert_eq!(tasks[0].status, "addressing");
        assert_eq!(tasks[0].claimed_by, None);
        assert_eq!(tasks[0].lease_until, None);
        assert_eq!(tasks[0].last_heartbeat, None);

        teardown(previous);
    }

    #[tokio::test]
    async fn runtime_doctor_checks_detect_missing_active_artifacts() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        let database_path = current_database_path().await.unwrap();
        let mut checks = Vec::new();

        add_runtime_doctor_checks(&mut checks, &database_path).await;

        assert!(checks.iter().any(|check| {
            !check.ok && check.message == "active task path exists at .ferrus/tasks/t-001.md"
        }));
        assert!(checks.iter().any(|check| {
            !check.ok && check.message == "active run directory exists at .ferrus/runs/t-001"
        }));

        teardown(previous);
    }

    #[tokio::test]
    async fn runtime_doctor_checks_accept_consistent_active_task() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        tokio::fs::create_dir_all(".ferrus/tasks").await.unwrap();
        tokio::fs::write(".ferrus/tasks/t-001.md", "task")
            .await
            .unwrap();
        tokio::fs::create_dir_all(".ferrus/runs/t-001")
            .await
            .unwrap();
        let database_path = current_database_path().await.unwrap();
        let mut checks = Vec::new();

        add_runtime_doctor_checks(&mut checks, &database_path).await;

        assert!(
            checks.iter().all(|check| check.ok),
            "unexpected failed checks: {:?}",
            checks
                .iter()
                .filter(|check| !check.ok)
                .map(|check| check.message.as_str())
                .collect::<Vec<_>>()
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn recover_expired_task_leases_releases_stale_claims() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        claim_current_task("executor:codex:1", 0).await.unwrap();

        let recovered = recover_expired_task_leases().await.unwrap();
        let tasks = list_tasks().await.unwrap();
        let events = list_events(10, None).await.unwrap();

        assert_eq!(recovered, 1);
        assert_eq!(tasks[0].claimed_by, None);
        assert_eq!(tasks[0].lease_until, None);
        assert_eq!(tasks[0].last_heartbeat, None);
        assert!(
            events
                .iter()
                .any(|event| event.event_type == "task_lease_expired")
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn recover_runtime_state_clears_expired_state_lease_mirror() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        claim_current_task("executor:codex:1", 0).await.unwrap();
        let mut state = store::read_state().await.unwrap();
        state.claimed_by = Some("executor:codex:1".to_string());
        state.lease_until = Some(Utc::now() - chrono::Duration::seconds(1));
        state.last_heartbeat = Some(Utc::now() - chrono::Duration::seconds(2));
        store::write_state(&state).await.unwrap();

        let recovery = recover_runtime_state().await.unwrap();
        let state = store::read_state().await.unwrap();
        let tasks = list_tasks().await.unwrap();
        let events = list_events(20, None).await.unwrap();

        assert_eq!(recovery.expired_task_leases, 1);
        assert_eq!(recovery.state_lease_mirrors_cleared, 1);
        assert_eq!(state.claimed_by, None);
        assert_eq!(state.lease_until, None);
        assert_eq!(state.last_heartbeat, None);
        assert_eq!(tasks[0].claimed_by, None);
        assert!(
            events
                .iter()
                .any(|event| { event.event_type == "state_lease_mirror_cleared" })
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn preview_orphaned_worktrees_ignores_active_tasks_and_runs() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (dir, previous) = setup_project().await;
        let workspace = dir.path();
        let worktrees_dir = workspace.join(".ferrus/projects/test-project/worktrees");
        tokio::fs::create_dir_all(worktrees_dir.join("t-active"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(worktrees_dir.join("t-run"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(worktrees_dir.join("t-orphan"))
            .await
            .unwrap();

        record_task_status("t-active", ".ferrus/tasks/t-active.md", "addressing")
            .await
            .unwrap();
        let run = record_run_started_with_workspace(
            "executor-run-t-run",
            "executor",
            "executor:codex:t-run",
            std::process::id(),
            path_string(&worktrees_dir.join("t-run")),
        )
        .await
        .unwrap();
        record_task_status("t-run", ".ferrus/tasks/t-run.md", "complete")
            .await
            .unwrap();
        let attached =
            attach_running_run_to_task("executor:codex:t-run", "t-run", ".ferrus/tasks/t-run.md")
                .await
                .unwrap();
        assert_eq!(attached.as_deref(), Some(run.id.as_str()));

        let registration = ProjectRegistration {
            local_ref: LocalProjectRef {
                project_id: "test-project".to_string(),
                name: "test".to_string(),
                data_dir: path_string(&workspace.join(".ferrus/projects/test-project")),
            },
            metadata: ProjectMetadata {
                id: "test-project".to_string(),
                name: "test".to_string(),
                workspace_dir: path_string(workspace),
                ferrus_dir: path_string(&workspace.join(".ferrus")),
                vcs: None,
                origin_repo: None,
                default_branch: None,
                current_head: None,
                created_at: "2026-05-16T10:00:00Z".to_string(),
                last_opened_at: "2026-05-16T10:00:00Z".to_string(),
                version: PROJECT_VERSION,
            },
            data_dir: workspace.join(".ferrus/projects/test-project"),
            database_path: workspace.join(".ferrus/projects/test-project/ferrus.db"),
        };
        let orphaned = orphaned_worktrees_for(&registration).await.unwrap();

        assert_eq!(orphaned, vec![worktrees_dir.join("t-orphan")]);

        teardown(previous);
    }

    #[tokio::test]
    async fn preview_runtime_recovery_reports_pending_work_without_mutating() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        claim_current_task("executor:codex:1", 0).await.unwrap();
        let mut state = store::read_state().await.unwrap();
        state.claimed_by = Some("executor:codex:1".to_string());
        state.lease_until = Some(Utc::now() - chrono::Duration::seconds(1));
        state.last_heartbeat = Some(Utc::now() - chrono::Duration::seconds(2));
        store::write_state(&state).await.unwrap();
        let database_path = current_database_path().await.unwrap();
        let mut checks = Vec::new();

        let preview = preview_runtime_recovery_from(&database_path).await.unwrap();
        add_recovery_doctor_checks(&mut checks, &database_path).await;
        let state_after = store::read_state().await.unwrap();
        let tasks = list_tasks().await.unwrap();

        assert_eq!(preview.interrupted_runs, 0);
        assert_eq!(preview.expired_task_leases, 1);
        assert_eq!(preview.state_lease_mirrors_cleared, 1);
        assert_eq!(state_after.claimed_by.as_deref(), Some("executor:codex:1"));
        assert_eq!(tasks[0].claimed_by.as_deref(), Some("executor:codex:1"));
        assert!(checks.iter().any(|check| {
            !check.ok
                && check
                    .message
                    .contains("expired task lease recovery pending (1")
        }));
        assert!(checks.iter().any(|check| {
            !check.ok
                && check
                    .message
                    .contains("stale STATE.json lease mirror recovery pending (1")
        }));

        teardown(previous);
    }

    #[tokio::test]
    async fn list_runs_and_events_reads_runtime_rows() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;

        let run = record_run_started("executor", "executor:codex:1", std::process::id())
            .await
            .unwrap();
        record_task_status("t-002", ".ferrus/tasks/t-002.md", "executing")
            .await
            .unwrap();
        let attached =
            attach_running_run_to_task("executor:codex:1", "t-002", ".ferrus/tasks/t-002.md")
                .await
                .unwrap();
        assert_eq!(attached.as_deref(), Some(run.id.as_str()));
        record_runtime_event(
            Some(run.id.clone()),
            "test_event",
            serde_json::json!({ "ok": true }),
        )
        .await
        .unwrap();
        record_run_finished(&run.id, 0).await.unwrap();

        let runs = list_runs(10).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].id, run.id);
        assert_eq!(runs[0].task_id, "t-002");
        assert_eq!(runs[0].role, "executor");
        assert_eq!(runs[0].agent, "executor:codex:1");
        assert_eq!(runs[0].status, "completed");
        assert!(runs[0].pid.is_none());
        assert!(!runs[0].started_at.is_empty());
        assert!(!runs[0].updated_at.is_empty());

        let events = list_events(10, Some(run.id.clone())).await.unwrap();
        assert!(events.iter().any(|event| event.event_type == "run_started"));
        assert!(
            events
                .iter()
                .any(|event| event.event_type == "run_task_attached")
        );
        assert!(events.iter().any(|event| event.event_type == "test_event"));
        assert!(
            events
                .iter()
                .any(|event| event.event_type == "run_finished")
        );
        assert!(
            events
                .iter()
                .all(|event| event.run_id.as_deref() == Some(run.id.as_str()))
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn record_run_started_can_use_preallocated_run_id() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        let run_id = allocate_run_id("executor", "executor:codex:t-002");

        let run = record_run_started_with_id(
            &run_id,
            "executor",
            "executor:codex:t-002",
            std::process::id(),
        )
        .await
        .unwrap();

        assert_eq!(run.id, run_id);
        let runs = list_runs(10).await.unwrap();
        assert!(runs.iter().any(|run| run.id == run_id));

        teardown(previous);
    }

    #[tokio::test]
    async fn record_run_started_can_store_explicit_workspace_path() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (dir, previous) = setup_project().await;
        let run_id = allocate_run_id("executor", "executor:codex:t-003");
        let workspace_path = path_string(&dir.path().join("worktrees").join("t-003"));

        let run = record_run_started_with_workspace(
            &run_id,
            "executor",
            "executor:codex:t-003",
            std::process::id(),
            workspace_path.clone(),
        )
        .await
        .unwrap();

        assert_eq!(run.id, run_id);
        assert_eq!(run.workspace_path, workspace_path);
        let runs = list_runs(10).await.unwrap();
        assert!(
            runs.iter()
                .any(|run| run.id == run_id && run.workspace_path == workspace_path)
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn consultation_attachment_is_exclusive_to_one_supervisor_run() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        record_task_status("t-007", ".ferrus/tasks/t-007.md", "executing")
            .await
            .unwrap();
        record_task_consultation_requested("t-007", "executing")
            .await
            .unwrap();
        let first_run = record_run_started("supervisor", "supervisor:codex:1", std::process::id())
            .await
            .unwrap();
        let second_run = record_run_started("supervisor", "supervisor:codex:2", std::process::id())
            .await
            .unwrap();

        let first = attach_running_run_to_next_consultation("supervisor:codex:1")
            .await
            .unwrap();
        let second = attach_running_run_to_next_consultation("supervisor:codex:2")
            .await
            .unwrap();

        assert_eq!(
            first.as_ref().map(|context| context.task_id.as_str()),
            Some("t-007")
        );
        assert!(second.is_none());

        let runs = list_runs(10).await.unwrap();
        let first = runs.iter().find(|run| run.id == first_run.id).unwrap();
        let second = runs.iter().find(|run| run.id == second_run.id).unwrap();
        assert_eq!(first.task_id, "t-007");
        assert_eq!(second.task_id, "t-001");

        teardown(previous);
    }

    #[tokio::test]
    async fn targeted_consultation_attachment_does_not_steal_another_task() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_project().await;
        record_task_status("t-007", ".ferrus/tasks/t-007.md", "executing")
            .await
            .unwrap();
        record_task_status("t-008", ".ferrus/tasks/t-008.md", "executing")
            .await
            .unwrap();
        record_task_consultation_requested("t-007", "executing")
            .await
            .unwrap();
        record_task_consultation_requested("t-008", "executing")
            .await
            .unwrap();
        let first_run =
            record_run_started("supervisor", "supervisor:codex:t-008", std::process::id())
                .await
                .unwrap();
        let second_run =
            record_run_started("supervisor", "supervisor:codex:t-009", std::process::id())
                .await
                .unwrap();

        let first = attach_running_run_to_consultation("t-008", "supervisor:codex:t-008")
            .await
            .unwrap();
        let second = attach_running_run_to_consultation("t-009", "supervisor:codex:t-009")
            .await
            .unwrap();

        assert_eq!(
            first.as_ref().map(|context| context.task_id.as_str()),
            Some("t-008")
        );
        assert!(second.is_none());

        let runs = list_runs(10).await.unwrap();
        let first = runs.iter().find(|run| run.id == first_run.id).unwrap();
        let second = runs.iter().find(|run| run.id == second_run.id).unwrap();
        assert_eq!(first.task_id, "t-008");
        assert_eq!(second.task_id, "t-001");

        teardown(previous);
    }

    #[tokio::test]
    async fn list_registered_projects_reads_valid_and_invalid_entries() {
        let dir = TempDir::new().unwrap();
        let projects_dir = dir.path().join("projects");
        let valid_dir = projects_dir.join("PVALID");
        let invalid_dir = projects_dir.join("PBROKEN");
        std::fs::create_dir_all(&valid_dir).unwrap();
        std::fs::create_dir_all(&invalid_dir).unwrap();
        std::fs::write(valid_dir.join("ferrus.db"), "").unwrap();
        write_toml(
            &valid_dir.join("project.toml"),
            &ProjectMetadata {
                id: "PVALID".to_string(),
                name: "ferrus".to_string(),
                workspace_dir: "/tmp/ferrus".to_string(),
                ferrus_dir: "/tmp/ferrus/.ferrus".to_string(),
                vcs: Some("git".to_string()),
                origin_repo: None,
                default_branch: Some("main".to_string()),
                current_head: None,
                created_at: "2026-05-16T10:00:00Z".to_string(),
                last_opened_at: "2026-05-17T10:00:00Z".to_string(),
                version: PROJECT_VERSION,
            },
        )
        .await
        .unwrap();
        std::fs::write(invalid_dir.join("project.toml"), "not = [toml").unwrap();

        let projects = list_registered_projects_from(&projects_dir).await.unwrap();

        assert_eq!(projects.len(), 2);
        let valid = projects
            .iter()
            .find(|project| project.id == "PVALID")
            .unwrap();
        assert_eq!(valid.name.as_deref(), Some("ferrus"));
        assert_eq!(valid.workspace_dir.as_deref(), Some("/tmp/ferrus"));
        assert!(valid.database_exists);
        assert!(valid.error.is_none());

        let invalid = projects
            .iter()
            .find(|project| project.id == "PBROKEN")
            .unwrap();
        assert!(invalid.name.is_none());
        assert!(!invalid.database_exists);
        assert!(invalid.error.is_some());
    }

    #[tokio::test]
    async fn touch_current_project_updates_last_opened_without_rewriting_local_ref() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (dir, previous) = setup_project().await;
        let workspace = dir.path();
        let data_dir = workspace.join(".ferrus/projects/test-project");
        let metadata_path = data_dir.join("project.toml");
        let created_at = "2026-05-16T10:00:00Z";
        write_toml(
            &metadata_path,
            &ProjectMetadata {
                id: "test-project".to_string(),
                name: "old-name".to_string(),
                workspace_dir: "/old/workspace".to_string(),
                ferrus_dir: "/old/workspace/.ferrus".to_string(),
                vcs: None,
                origin_repo: None,
                default_branch: None,
                current_head: None,
                created_at: created_at.to_string(),
                last_opened_at: "2026-05-16T11:00:00Z".to_string(),
                version: PROJECT_VERSION,
            },
        )
        .await
        .unwrap();
        let local_ref_before = tokio::fs::read_to_string(workspace.join(".ferrus/project.toml"))
            .await
            .unwrap();

        let registration = touch_current_project().await.unwrap();
        let metadata = read_project_metadata_from(&metadata_path).await.unwrap();
        let local_ref_after = tokio::fs::read_to_string(workspace.join(".ferrus/project.toml"))
            .await
            .unwrap();
        let canonical_workspace = tokio::fs::canonicalize(workspace).await.unwrap();

        assert_eq!(registration.local_ref.project_id, "test-project");
        assert_eq!(metadata.id, "test-project");
        assert_eq!(metadata.created_at, created_at);
        assert_ne!(metadata.last_opened_at, "2026-05-16T11:00:00Z");
        assert_eq!(metadata.workspace_dir, path_string(&canonical_workspace));
        assert_eq!(local_ref_after, local_ref_before);

        teardown(previous);
    }
}
