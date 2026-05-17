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

#[derive(Debug, Clone)]
pub enum LeaseRenewal {
    Renewed {
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
    if let Ok(state) = crate::state::store::read_state().await {
        record_current_task_status_best_effort(task_status_for_state(&state.state)).await;
    }
    Ok(registration)
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
            let run_dir = runs_dir.join(&id);
            return Ok(TaskArtifact {
                id,
                path: path_string(&task_path),
                run_dir: path_string(&run_dir),
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
        if matches!(status.as_str(), "idle" | "reset" | "complete" | "failed") {
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

pub async fn claim_current_task(agent_id: &str, ttl_secs: u64) -> Result<TaskClaim> {
    let database_path = current_database_path().await?;
    let (task_id, task_path) = current_task_identity().await;
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
        let lease_until_text = lease_until.to_rfc3339_opts(SecondsFormat::Secs, true);
        let now_text = now.to_rfc3339_opts(SecondsFormat::Secs, true);
        transaction.execute(
            "UPDATE tasks SET claimed_by = ?1, lease_until = ?2, last_heartbeat = ?3 WHERE id = ?4",
            params![agent_id, lease_until_text, now_text, task_id],
        )?;
        insert_event_in_transaction(
            &transaction,
            None,
            "task_claimed",
            &serde_json::json!({
                "task_id": task_id,
                "claimed_by": agent_id,
                "lease_until": lease_until,
            }),
        )?;
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
    let (task_id, _) = current_task_identity().await;
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
        let now = Utc::now();
        let existing_lease = lease_until
            .as_deref()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc));
        if existing_lease.is_none_or(|lease_until| now >= lease_until) {
            transaction.commit()?;
            return Ok(LeaseRenewal::Expired);
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
            &transaction,
            None,
            "task_lease_renewed",
            &serde_json::json!({
                "task_id": task_id,
                "claimed_by": agent_id,
                "lease_until": lease_until,
            }),
        )?;
        transaction.commit()?;
        Ok(LeaseRenewal::Renewed {
            claimed_by: agent_id,
            lease_until,
        })
    })
    .await?
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
    Ok(RuntimeRecovery {
        interrupted_runs,
        expired_task_leases,
    })
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
        std::fs::write(
            workspace.join(".ferrus/project.toml"),
            format!(
                "project_id = \"test-project\"\nname = \"test\"\ndata_dir = \"{}\"\n",
                data_dir.display()
            ),
        )
        .unwrap();
        std::env::set_current_dir(workspace).unwrap();
        initialize_database(&data_dir.join("ferrus.db"))
            .await
            .unwrap();

        let mut state = StateData::default();
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
}
