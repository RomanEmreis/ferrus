use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::path::Path;

use crate::runtime_status::TaskStatus;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub enum LegacyTaskState {
    Idle,
    Executing,
    Consultation,
    Reviewing,
    Addressing,
    Complete,
    Failed,
    AwaitingHuman,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LegacyStateData {
    #[serde(default)]
    pub state: Option<LegacyTaskState>,
    #[serde(default)]
    pub task_spec: Option<String>,
    #[serde(default)]
    pub task_milestone: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    pub selected_spec: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    pub selected_milestone: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
}

impl LegacyStateData {
    pub fn state(&self) -> LegacyTaskState {
        self.state.clone().unwrap_or(LegacyTaskState::Idle)
    }
}

pub async fn read_legacy_state(path: impl AsRef<Path>) -> Result<LegacyStateData> {
    let path = path.as_ref();
    let contents = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("Failed to read legacy {}", path.display()))?;
    serde_json::from_str(&contents)
        .with_context(|| format!("Failed to parse legacy {}", path.display()))
}

pub fn task_status_for_legacy_state(state: &LegacyTaskState) -> TaskStatus {
    match state {
        LegacyTaskState::Idle => TaskStatus::Reset,
        LegacyTaskState::Executing => TaskStatus::Executing,
        LegacyTaskState::Consultation => TaskStatus::Consultation,
        LegacyTaskState::Reviewing => TaskStatus::Reviewing,
        LegacyTaskState::Addressing => TaskStatus::Addressing,
        LegacyTaskState::Complete => TaskStatus::Complete,
        LegacyTaskState::Failed => TaskStatus::Failed,
        LegacyTaskState::AwaitingHuman => TaskStatus::AwaitingHuman,
    }
}
