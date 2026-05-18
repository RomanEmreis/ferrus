use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::warn;

use crate::{platform, state::machine::TaskState};

const PROJECT_VERSION: u32 = 1;
const LOCAL_PROJECT_TOML: &str = ".ferrus/project.toml";
const CURRENT_TASK_ID: &str = "current";
const CURRENT_TASK_PATH: &str = ".ferrus/TASK.md";

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

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone)]
pub struct TaskRecord {
    pub id: String,
    pub path: String,
    pub status: String,
    pub claimed_by: Option<String>,
    pub lease_until: Option<String>,
    pub last_heartbeat: Option<String>,
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
    write_toml(Path::new(LOCAL_PROJECT_TOML), &local_ref).await?;

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
            SELECT id, path, status, claimed_by, lease_until, last_heartbeat
            FROM tasks
            ORDER BY
                CASE WHEN id = 'current' THEN 0 ELSE 1 END,
                id
            "#,
        )?;
        let rows = statement.query_map([], |row| {
            Ok(TaskRecord {
                id: row.get(0)?,
                path: row.get(1)?,
                status: row.get(2)?,
                claimed_by: row.get(3)?,
                lease_until: row.get(4)?,
                last_heartbeat: row.get(5)?,
            })
        })?;

        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row?);
        }
        Ok(tasks)
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

pub async fn record_current_task_status(status: &str) -> Result<()> {
    let (task_id, task_path) = current_task_identity().await;
    record_task_status(&task_id, &task_path, status).await
}

pub async fn record_task_status(task_id: &str, task_path: &str, status: &str) -> Result<()> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    let task_path = task_path.to_string();
    let status = status.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = open_runtime_database(&database_path)?;
        upsert_task(&connection, &task_id, &task_path, &status)?;
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
    let database_path = current_database_path().await?;
    let agent_id = agent_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<ReadyTaskClaim> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction()?;
        let now = Utc::now();
        let candidates = ready_task_candidates(&transaction)?;

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

pub async fn record_run_started(role: &str, agent: &str, pid: u32) -> Result<RunRecord> {
    let database_path = current_database_path().await?;
    let workspace_path = path_string(&canonical_current_dir().await?);
    let (task_id, task_path) = current_task_identity().await;
    let role = role.to_string();
    let agent = agent.to_string();
    let run_id = generate_run_id(&role, &agent, pid);
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

pub async fn record_run_started_best_effort(role: &str, agent: &str, pid: u32) -> Option<String> {
    match record_run_started(role, agent, pid).await {
        Ok(record) => Some(record.id),
        Err(err) => {
            warn!(error = ?err, role, agent, pid, "failed to mirror run start into ferrus.db");
            None
        }
    }
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
        for column in ["claimed_by", "lease_until", "last_heartbeat"] {
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
                SELECT id, path, status, claimed_by, lease_until, last_heartbeat
                FROM tasks
                WHERE id = ?1
                "#,
                [task_id],
                |row| {
                    Ok(TaskRecord {
                        id: row.get(0)?,
                        path: row.get(1)?,
                        status: row.get(2)?,
                        claimed_by: row.get(3)?,
                        lease_until: row.get(4)?,
                        last_heartbeat: row.get(5)?,
                    })
                },
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

async fn current_task_identity() -> (String, String) {
    let Ok(state) = crate::state::store::read_state().await else {
        return (CURRENT_TASK_ID.to_string(), CURRENT_TASK_PATH.to_string());
    };
    (
        state
            .active_task_id
            .unwrap_or_else(|| CURRENT_TASK_ID.to_string()),
        state
            .active_task_path
            .unwrap_or_else(|| CURRENT_TASK_PATH.to_string()),
    )
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
            claimed_by TEXT,
            lease_until TEXT,
            last_heartbeat TEXT
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
    ensure_column(connection, "tasks", "claimed_by", "TEXT")?;
    ensure_column(connection, "tasks", "lease_until", "TEXT")?;
    ensure_column(connection, "tasks", "last_heartbeat", "TEXT")?;
    Ok(())
}

fn upsert_task(connection: &Connection, id: &str, path: &str, status: &str) -> Result<()> {
    connection.execute(
        r#"
        INSERT INTO tasks (id, path, status)
        VALUES (?1, ?2, ?3)
        ON CONFLICT(id) DO UPDATE SET
            path = excluded.path,
            status = excluded.status
        "#,
        params![id, path, status],
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

struct ReadyTaskCandidate {
    id: String,
    path: String,
    status: String,
    claimed_by: Option<String>,
    lease_until: Option<String>,
}

fn ready_task_candidates(transaction: &Transaction<'_>) -> Result<Vec<ReadyTaskCandidate>> {
    let mut statement = transaction.prepare(
        r#"
        SELECT id, path, status, claimed_by, lease_until
        FROM tasks
        WHERE status IN ('executing', 'addressing')
        ORDER BY id
        "#,
    )?;
    let rows = statement.query_map([], |row| {
        Ok(ReadyTaskCandidate {
            id: row.get(0)?,
            path: row.get(1)?,
            status: row.get(2)?,
            claimed_by: row.get(3)?,
            lease_until: row.get(4)?,
        })
    })?;

    let mut tasks = Vec::new();
    for row in rows {
        tasks.push(row?);
    }
    Ok(tasks)
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
    let contents = tokio::fs::read_to_string(LOCAL_PROJECT_TOML)
        .await
        .context("Failed to read .ferrus/project.toml")?;
    toml::from_str(&contents).context("Failed to parse .ferrus/project.toml")
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

fn generate_run_id(role: &str, agent: &str, pid: u32) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let mut hasher = DefaultHasher::new();
    role.hash(&mut hasher);
    agent.hash(&mut hasher);
    pid.hash(&mut hasher);
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

        let run = record_run_started("executor", "codex", std::process::id())
            .await
            .unwrap();
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
        assert_eq!(runs[0].task_id, "t-001");
        assert_eq!(runs[0].role, "executor");
        assert_eq!(runs[0].agent, "codex");
        assert_eq!(runs[0].status, "completed");
        assert!(runs[0].pid.is_none());
        assert!(!runs[0].started_at.is_empty());
        assert!(!runs[0].updated_at.is_empty());

        let events = list_events(10, Some(run.id.clone())).await.unwrap();
        assert!(events.iter().any(|event| event.event_type == "run_started"));
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
