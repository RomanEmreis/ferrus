use anyhow::{Context, Result};
use neva::prelude::*;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
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

    let content = ensure_trailing_newline(markdown);
    let path = create_unique_spec_file(&spec_dir, &base_name, content.as_bytes()).await?;

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

async fn create_unique_spec_file(dir: &Path, base_name: &str, content: &[u8]) -> Result<PathBuf> {
    let mut candidate = spec_path_candidate(dir, base_name, 1);
    let mut suffix = 2;
    loop {
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
            .await
        {
            Ok(mut file) => {
                file.write_all(content)
                    .await
                    .with_context(|| format!("Failed to write spec {}", candidate.display()))?;
                file.flush()
                    .await
                    .with_context(|| format!("Failed to flush spec {}", candidate.display()))?;
                return Ok(candidate);
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                candidate = spec_path_candidate(dir, base_name, suffix);
                suffix += 1;
            }
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("Failed to create spec {}", candidate.display()));
            }
        }
    }
}

fn spec_path_candidate(dir: &Path, base_name: &str, suffix: u32) -> PathBuf {
    if suffix == 1 {
        dir.join(format!("{base_name}.md"))
    } else {
        dir.join(format!("{base_name}-{suffix}.md"))
    }
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

    #[test]
    fn spec_path_candidate_adds_suffix_after_first_candidate() {
        let dir = Path::new("docs/specs");
        assert_eq!(
            spec_path_candidate(dir, "2026-04-26-feature", 1),
            dir.join("2026-04-26-feature.md")
        );
        assert_eq!(
            spec_path_candidate(dir, "2026-04-26-feature", 2),
            dir.join("2026-04-26-feature-2.md")
        );
    }
}
