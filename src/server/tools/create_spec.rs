use anyhow::{Context, Result};
use neva::prelude::*;
use std::path::{Path, PathBuf};
use tracing::info;

use crate::{config::Config, state::store};

use super::tool_err;

pub const DESCRIPTION: &str = "Create an approved feature specification. Writes Markdown to the \
     configured [spec].directory and records the created path for Ferrus HQ. Must only be called \
     after explicit user approval of the final spec text.";

pub const INPUT_SCHEMA: &str = r#"{
    "properties": {
        "markdown": {
            "type": "string",
            "description": "Full approved feature specification in Markdown, following ferrus://spec_template"
        }
    },
    "required": ["markdown"]
}"#;

pub async fn handler(markdown: String) -> Result<String, Error> {
    run(markdown).await.map_err(tool_err)
}

async fn run(markdown: String) -> Result<String> {
    let _state = store::read_state().await?;
    store::clear_last_spec_path().await?;

    if markdown.trim().is_empty() {
        anyhow::bail!("Cannot create spec: markdown content is empty.");
    }

    let config = Config::load().await?;
    let directory = config.spec.directory.trim();
    if directory.is_empty() {
        anyhow::bail!("Cannot create spec: ferrus.toml [spec].directory is empty.");
    }

    let spec_dir = PathBuf::from(directory);
    tokio::fs::create_dir_all(&spec_dir)
        .await
        .with_context(|| format!("Failed to create spec directory {}", spec_dir.display()))?;

    let title = extract_title(&markdown).unwrap_or("spec");
    let slug = slugify(title);
    let date = chrono::Utc::now().format("%Y-%m-%d");
    let base_name = format!("{date}-{slug}");
    let path = unique_spec_path(&spec_dir, &base_name).await;

    let content = ensure_trailing_newline(markdown);
    tokio::fs::write(&path, content)
        .await
        .with_context(|| format!("Failed to write spec {}", path.display()))?;

    let display_path = path.display().to_string();
    store::write_last_spec_path(&display_path).await?;

    info!("Spec created at {}", display_path);
    Ok(format!("Spec created at {display_path}."))
}

fn extract_title(markdown: &str) -> Option<&str> {
    markdown.lines().find_map(|line| {
        let trimmed = line.trim();
        trimmed
            .strip_prefix("# ")
            .map(str::trim)
            .filter(|title| !title.is_empty())
    })
}

fn slugify(title: &str) -> String {
    let mut slug = String::new();
    let mut previous_dash = false;

    for ch in title.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            previous_dash = false;
        } else if !previous_dash && !slug.is_empty() {
            slug.push('-');
            previous_dash = true;
        }
    }

    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "spec".to_string()
    } else {
        slug.to_string()
    }
}

async fn unique_spec_path(dir: &Path, base_name: &str) -> PathBuf {
    let mut candidate = dir.join(format!("{base_name}.md"));
    let mut suffix = 2;
    while tokio::fs::try_exists(&candidate).await.unwrap_or(false) {
        candidate = dir.join(format!("{base_name}-{suffix}.md"));
        suffix += 1;
    }
    candidate
}

fn ensure_trailing_newline(mut markdown: String) -> String {
    if !markdown.ends_with('\n') {
        markdown.push('\n');
    }
    markdown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_first_h1_title() {
        let markdown = "intro\n# Feature Spec\n\n## Goal\n...";
        assert_eq!(extract_title(markdown), Some("Feature Spec"));
    }

    #[test]
    fn slugifies_title_for_filename() {
        assert_eq!(slugify("Ferrus /spec: v2!"), "ferrus-spec-v2");
    }

    #[test]
    fn slugify_falls_back_for_non_ascii_title() {
        assert_eq!(slugify("спека"), "spec");
    }
}
