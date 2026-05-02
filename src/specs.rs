use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::{config::Config, state::machine::StateData};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Milestone {
    pub marker: String,
    pub id: String,
    pub title: String,
    pub depends_on: String,
    pub completed: bool,
    line_index: usize,
}

impl Milestone {
    pub fn display_title(&self) -> String {
        format!("{} {}", self.marker, self.title)
    }
}

#[derive(Clone, Debug)]
pub struct SpecDocument {
    pub path: String,
    pub milestones: Vec<Milestone>,
    lines: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedMilestone {
    pub spec_path: String,
    pub spec_display: String,
    pub milestone: Milestone,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectedMilestoneState {
    MissingSelection,
    SpecMissing(String),
    MilestoneMissing(String),
    Found(SelectedMilestone),
}

pub async fn list_spec_paths() -> Result<Vec<String>> {
    let config = Config::load().await?;
    let directory = config.spec.directory.trim();
    if directory.is_empty() {
        anyhow::bail!("ferrus.toml [spec].directory is empty.");
    }

    let mut entries = tokio::fs::read_dir(directory)
        .await
        .with_context(|| format!("Failed to read spec directory {directory}"))?;
    let mut paths = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .with_context(|| format!("Failed to read spec directory {directory}"))?
    {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
            paths.push(path.display().to_string());
        }
    }
    paths.sort();
    Ok(paths)
}

pub async fn load_spec(path: &str) -> Result<SpecDocument> {
    let content = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("Failed to read spec {path}"))?;
    Ok(parse_spec(path, &content))
}

pub fn parse_spec(path: &str, content: &str) -> SpecDocument {
    let lines = content
        .split_inclusive('\n')
        .map(|line| line.trim_end_matches('\n').to_string())
        .collect::<Vec<_>>();
    let mut milestones = Vec::new();
    let mut idx = 0;

    while idx < lines.len() {
        if let Some((completed, marker, title)) = parse_milestone_header(&lines[idx]) {
            let mut id = None;
            let mut depends_on = None;
            let mut child = idx + 1;
            while child < lines.len() {
                if parse_milestone_header(&lines[child]).is_some() {
                    break;
                }
                if let Some(value) = parse_child_field(&lines[child], "ID") {
                    id = Some(value.to_string());
                } else if let Some(value) = parse_child_field(&lines[child], "Depends on") {
                    depends_on = Some(value.to_string());
                }
                child += 1;
            }
            if let Some(id) = id.filter(|id| !id.is_empty()) {
                milestones.push(Milestone {
                    marker: marker.to_string(),
                    id,
                    title: title.to_string(),
                    depends_on: depends_on.unwrap_or_else(|| "none".to_string()),
                    completed,
                    line_index: idx,
                });
            }
            idx = child;
        } else {
            idx += 1;
        }
    }

    SpecDocument {
        path: path.to_string(),
        milestones,
        lines,
    }
}

pub async fn select_first_incomplete(state: &mut StateData, spec_path: &str) -> Result<()> {
    let spec = load_spec(spec_path).await?;
    state.selected_spec = Some(spec_path.to_string());
    state.selected_milestone = spec
        .milestones
        .iter()
        .find(|milestone| !milestone.completed)
        .map(|milestone| milestone.id.clone());
    Ok(())
}

pub async fn resolve_selected(state: &StateData) -> Result<SelectedMilestoneState> {
    let Some(spec_path) = state
        .selected_spec
        .as_deref()
        .filter(|path| !path.is_empty())
    else {
        return Ok(SelectedMilestoneState::MissingSelection);
    };
    let Some(milestone_id) = state
        .selected_milestone
        .as_deref()
        .filter(|id| !id.is_empty())
    else {
        return Ok(SelectedMilestoneState::MissingSelection);
    };

    if !Path::new(spec_path).exists() {
        return Ok(SelectedMilestoneState::SpecMissing(spec_path.to_string()));
    }

    let spec = load_spec(spec_path).await?;
    let Some(milestone) = spec
        .milestones
        .iter()
        .find(|milestone| milestone.id == milestone_id)
        .cloned()
    else {
        return Ok(SelectedMilestoneState::MilestoneMissing(
            milestone_id.to_string(),
        ));
    };

    Ok(SelectedMilestoneState::Found(SelectedMilestone {
        spec_path: spec.path.clone(),
        spec_display: spec_display_name(&spec.path),
        milestone,
    }))
}

pub async fn complete_selected_milestone_and_advance(state: &mut StateData) -> Result<()> {
    let Some(spec_path) = state.selected_spec.clone() else {
        return Ok(());
    };
    let Some(milestone_id) = state.selected_milestone.clone() else {
        return Ok(());
    };
    if !Path::new(&spec_path).exists() {
        return Ok(());
    }

    let mut spec = load_spec(&spec_path).await?;
    let Some(current_idx) = spec
        .milestones
        .iter()
        .position(|milestone| milestone.id == milestone_id)
    else {
        return Ok(());
    };

    if !spec.milestones[current_idx].completed {
        mark_line_completed(&mut spec.lines[spec.milestones[current_idx].line_index]);
        spec.milestones[current_idx].completed = true;
        write_spec_lines(&spec_path, &spec.lines).await?;
    }

    state.selected_milestone = spec
        .milestones
        .iter()
        .skip(current_idx + 1)
        .find(|milestone| !milestone.completed)
        .map(|milestone| milestone.id.clone());
    Ok(())
}

pub fn spec_display_name(path: &str) -> String {
    let stem = PathBuf::from(path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(path)
        .to_string();
    strip_date_prefix(&stem).unwrap_or(&stem).to_string()
}

pub fn compact_spec_display_name(name: &str) -> String {
    const MAX_CHARS: usize = 18;
    const ELLIPSIS: &str = "...";

    let char_count = name.chars().count();
    if char_count <= MAX_CHARS {
        return name.to_string();
    }

    let keep = MAX_CHARS.saturating_sub(ELLIPSIS.chars().count());
    format!(
        "{}{}",
        name.chars().take(keep).collect::<String>(),
        ELLIPSIS
    )
}

fn strip_date_prefix(stem: &str) -> Option<&str> {
    let bytes = stem.as_bytes();
    if bytes.len() > 11
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'-'
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..10].iter().all(u8::is_ascii_digit)
    {
        Some(&stem[11..])
    } else {
        None
    }
}

fn parse_milestone_header(line: &str) -> Option<(bool, &str, &str)> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("- [")?;
    let (mark, rest) = rest.split_once("] ")?;
    let completed = match mark {
        " " => false,
        "x" | "X" => true,
        _ => return None,
    };
    let rest = rest.trim();
    if !rest.starts_with('#') {
        return None;
    }
    let mut parts = rest.splitn(2, char::is_whitespace);
    let marker = parts.next()?.trim();
    let title = parts.next().unwrap_or_default().trim();
    if marker.len() <= 1 || title.is_empty() {
        return None;
    }
    Some((completed, marker, title))
}

fn parse_child_field<'a>(line: &'a str, field: &str) -> Option<&'a str> {
    let trimmed = line.trim();
    let trimmed = trimmed.strip_prefix("- ").unwrap_or(trimmed);
    let value = trimmed.strip_prefix(field)?.strip_prefix(':')?.trim();
    Some(value)
}

fn mark_line_completed(line: &mut String) {
    if let Some(pos) = line.find("- [ ]") {
        line.replace_range(pos..pos + 5, "- [x]");
    }
}

async fn write_spec_lines(path: &str, lines: &[String]) -> Result<()> {
    let mut content = lines.join("\n");
    if !content.ends_with('\n') {
        content.push('\n');
    }
    tokio::fs::write(path, content)
        .await
        .with_context(|| format!("Failed to write spec {path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_checkable_milestones_with_ids() {
        let spec = parse_spec(
            "docs/specs/2026-04-26-spec-workflow.md",
            "## Milestones\n\
             - [ ] #1.0 Define spec workflow\n\
               - ID: m1.0\n\
               - Depends on: none\n\n\
             - [x] #1.1 Implement /spec command\n\
               - ID: m1.1\n\
               - Depends on: #1.0\n",
        );

        assert_eq!(spec.milestones.len(), 2);
        assert_eq!(spec.milestones[0].id, "m1.0");
        assert_eq!(spec.milestones[0].depends_on, "none");
        assert!(!spec.milestones[0].completed);
        assert_eq!(
            spec.milestones[1].display_title(),
            "#1.1 Implement /spec command"
        );
        assert!(spec.milestones[1].completed);
    }

    #[test]
    fn strips_date_prefix_for_display() {
        assert_eq!(
            spec_display_name("docs/specs/2026-04-26-spec-workflow.md"),
            "spec-workflow"
        );
    }

    #[test]
    fn compacts_long_spec_display_names() {
        assert_eq!(compact_spec_display_name("spec-workflow"), "spec-workflow");
        assert_eq!(
            compact_spec_display_name("spec-workflow-with-a-long-title"),
            "spec-workflow-w..."
        );
    }

    #[tokio::test]
    async fn completes_selected_milestone_and_advances_to_next_open() {
        let dir = std::env::temp_dir().join(format!(
            "ferrus-specs-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("2026-04-26-spec-workflow.md");
        std::fs::write(
            &path,
            "## Milestones\n\
             - [ ] #1.0 Define spec workflow\n\
               - ID: m1.0\n\
               - Depends on: none\n\n\
             - [ ] #1.1 Implement /spec command\n\
               - ID: m1.1\n\
               - Depends on: #1.0\n",
        )
        .unwrap();

        let mut state = StateData {
            selected_spec: Some(path.display().to_string()),
            selected_milestone: Some("m1.0".to_string()),
            ..StateData::default()
        };

        complete_selected_milestone_and_advance(&mut state)
            .await
            .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("- [x] #1.0 Define spec workflow"));
        assert_eq!(state.selected_milestone.as_deref(), Some("m1.1"));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
