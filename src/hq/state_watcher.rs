use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, Utc};
use tokio::sync::watch;

use crate::project::{self, ProjectSelection};
use crate::specs::{self, MilestoneReadiness};
use crate::state::{
    machine::{StateData, TaskState},
    store,
};

#[derive(Clone, Debug)]
pub struct TransitionSnapshot {
    pub from: TaskState,
    pub to: TaskState,
    pub elapsed: Duration,
    pub used_total: bool,
}

#[derive(Clone, Debug)]
pub struct WatchedState {
    pub state: StateData,
    pub state_elapsed: Duration,
    pub transition: Option<TransitionSnapshot>,
    pub selected_spec_display: Option<String>,
    pub selected_milestone_display: Option<String>,
    pub selected_milestones: Vec<WatchedMilestone>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchedMilestone {
    pub marker: String,
    pub title: String,
    pub completed: bool,
    pub readiness: MilestoneReadiness,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SelectedDisplayCacheKey {
    selected_spec: Option<String>,
    spec_fingerprint: Option<(Option<SystemTime>, u64)>,
}

#[derive(Clone, Debug, Default)]
struct SelectedDisplayCache {
    key: Option<SelectedDisplayCacheKey>,
    value: (Option<String>, Option<String>, Vec<WatchedMilestone>),
}

impl SelectedDisplayCache {
    async fn get(
        &mut self,
        selection: &ProjectSelection,
    ) -> (Option<String>, Option<String>, Vec<WatchedMilestone>) {
        let key = SelectedDisplayCacheKey {
            selected_spec: selection.selected_spec.clone(),
            spec_fingerprint: selected_spec_fingerprint(selection).await,
        };

        if self.key.as_ref() != Some(&key) {
            self.value = selected_milestone_display(selection).await;
            self.key = Some(key);
        }

        self.value.clone()
    }
}

/// Poll STATE.json every 250 ms and refresh elapsed timers every second.
///
/// `updated_at` is the source of truth for how long the current state has been
/// active. Total task time is tracked in-memory from the first observed
/// `Idle -> Executing` transition until the task returns to `Idle`.
pub async fn watch(tx: watch::Sender<Option<WatchedState>>) {
    let mut last_state: Option<StateData> = None;
    let mut last_sent_state_elapsed_secs = None;
    let mut last_sent_task_elapsed_secs = None;
    let mut task_started_at = None;
    let mut selected_display_cache = SelectedDisplayCache::default();

    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;

        let Ok(state) = store::read_state().await else {
            continue;
        };
        let selection =
            project::read_project_selection()
                .await
                .unwrap_or_else(|_| ProjectSelection {
                    selected_spec: state.selected_spec.clone(),
                });

        let now = Utc::now();
        let state_elapsed = elapsed_since(state.updated_at, now);
        let mut transition = None;

        if let Some(previous) = last_state.as_ref()
            && previous.state != state.state
        {
            if previous.state == TaskState::Idle && state.state == TaskState::Executing {
                task_started_at = Some(Instant::now());
            }

            let task_elapsed = task_started_at.map(|started| started.elapsed());
            let is_terminal = matches!(state.state, TaskState::Complete | TaskState::Failed);
            let elapsed = if is_terminal {
                task_elapsed.unwrap_or_else(|| elapsed_since(previous.updated_at, now))
            } else {
                elapsed_since(previous.updated_at, now)
            };

            transition = Some(TransitionSnapshot {
                from: previous.state.clone(),
                to: state.state.clone(),
                elapsed,
                used_total: is_terminal && task_elapsed.is_some(),
            });

            if state.state == TaskState::Idle {
                task_started_at = None;
            }
        }

        let task_elapsed = task_started_at.map(|started| started.elapsed());
        let state_elapsed_secs = state_elapsed.as_secs();
        let task_elapsed_secs = task_elapsed.map(|elapsed| elapsed.as_secs());
        let changed = last_state.as_ref().is_none_or(|previous| {
            previous.updated_at != state.updated_at || previous.state != state.state
        });

        if changed
            || transition.is_some()
            || last_sent_state_elapsed_secs != Some(state_elapsed_secs)
            || last_sent_task_elapsed_secs != task_elapsed_secs
        {
            last_sent_state_elapsed_secs = Some(state_elapsed_secs);
            last_sent_task_elapsed_secs = task_elapsed_secs;
            last_state = Some(state.clone());
            let (selected_spec_display, selected_milestone_display, selected_milestones) =
                selected_display_cache.get(&selection).await;
            let _ = tx.send(Some(WatchedState {
                state,
                state_elapsed,
                transition,
                selected_spec_display,
                selected_milestone_display,
                selected_milestones,
            }));
        }
    }
}

async fn selected_spec_fingerprint(
    selection: &ProjectSelection,
) -> Option<(Option<SystemTime>, u64)> {
    let path = selection.selected_spec.as_deref()?.trim();
    if path.is_empty() {
        return None;
    }

    let metadata = tokio::fs::metadata(path).await.ok()?;
    Some((metadata.modified().ok(), metadata.len()))
}

async fn selected_milestone_display(
    selection: &ProjectSelection,
) -> (Option<String>, Option<String>, Vec<WatchedMilestone>) {
    let selected_spec_display = selection
        .selected_spec
        .as_deref()
        .map(specs::spec_display_name)
        .map(|name| specs::compact_spec_display_name(&name))
        .filter(|name| !name.is_empty());
    let selected_milestones = match selection
        .selected_spec
        .as_deref()
        .filter(|path| !path.is_empty())
    {
        Some(path) => specs::load_spec(path)
            .await
            .map(|spec| {
                spec.milestone_plan()
                    .into_iter()
                    .map(|item| WatchedMilestone {
                        marker: item.milestone.marker,
                        title: item.milestone.title,
                        completed: item.milestone.completed,
                        readiness: item.readiness,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        None => Vec::new(),
    };

    let next_ready = selected_milestones
        .iter()
        .find(|milestone| milestone.readiness == MilestoneReadiness::Ready)
        .map(|milestone| milestone.marker.clone());
    (selected_spec_display, next_ready, selected_milestones)
}

fn elapsed_since(started_at: DateTime<Utc>, now: DateTime<Utc>) -> Duration {
    now.signed_duration_since(started_at)
        .to_std()
        .unwrap_or_default()
}

pub fn format_elapsed(duration: Duration) -> String {
    let total = duration.as_secs();
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;

    let mut parts = Vec::with_capacity(3);
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if minutes > 0 {
        parts.push(format!("{minutes}m"));
    }
    parts.push(format!("{seconds}s"));
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::format_elapsed;

    #[test]
    fn formats_seconds_only() {
        assert_eq!(format_elapsed(Duration::from_secs(45)), "45s");
    }

    #[test]
    fn formats_minutes_and_seconds() {
        assert_eq!(format_elapsed(Duration::from_secs(70)), "1m 10s");
    }

    #[test]
    fn formats_hours_minutes_and_seconds() {
        assert_eq!(format_elapsed(Duration::from_secs(5403)), "1h 30m 3s");
    }

    #[test]
    fn formats_zero_duration() {
        assert_eq!(format_elapsed(Duration::from_secs(0)), "0s");
    }
}
