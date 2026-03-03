use anyhow::{Context, Result};
use std::path::Path;

use crate::state::{machine::StateData, store};

const DEFAULT_FERRUS_TOML: &str = r#"[checks]
commands = [
    "cargo clippy -- -D warnings",
    "cargo fmt --check",
    "cargo test",
]

[limits]
max_check_retries = 5   # consecutive check failures before state → Failed
max_review_cycles = 3   # reject→fix cycles before state → Failed
max_feedback_lines = 30 # trailing lines per failing command in FEEDBACK.md (full output always in .ferrus/logs/)
"#;


pub async fn run() -> Result<()> {
    create_ferrus_toml().await?;
    create_ferrus_dir().await?;
    update_gitignore().await?;
    println!("\nferrus initialized. Run `ferrus serve` to start the MCP server.");
    Ok(())
}

async fn create_ferrus_toml() -> Result<()> {
    let path = Path::new("ferrus.toml");
    if path.exists() {
        println!("ferrus.toml already exists, skipping.");
    } else {
        tokio::fs::write(path, DEFAULT_FERRUS_TOML)
            .await
            .context("Failed to write ferrus.toml")?;
        println!("Created ferrus.toml");
    }
    Ok(())
}

async fn create_ferrus_dir() -> Result<()> {
    let dir = Path::new(".ferrus");
    tokio::fs::create_dir_all(dir.join("logs"))
        .await
        .context("Failed to create .ferrus/logs/ directory")?;

    let state_path = dir.join("STATE.json");
    if !state_path.exists() {
        store::write_state(&StateData::default())
            .await
            .context("Failed to write .ferrus/STATE.json")?;
        println!("Created .ferrus/STATE.json");
    }

    for filename in ["TASK.md", "FEEDBACK.md", "REVIEW.md", "SUBMISSION.md"] {
        let path = dir.join(filename);
        if !path.exists() {
            tokio::fs::write(&path, "")
                .await
                .with_context(|| format!("Failed to write .ferrus/{filename}"))?;
            println!("Created .ferrus/{filename}");
        }
    }
    Ok(())
}

async fn update_gitignore() -> Result<()> {
    let path = Path::new(".gitignore");
    let entry = ".ferrus/\n";
    if path.exists() {
        let contents = tokio::fs::read_to_string(path)
            .await
            .context("Failed to read .gitignore")?;
        if contents.contains(".ferrus/") {
            return Ok(());
        }
        tokio::fs::write(path, format!("{contents}{entry}"))
            .await
            .context("Failed to update .gitignore")?;
        println!("Added .ferrus/ to .gitignore");
    } else {
        tokio::fs::write(path, entry)
            .await
            .context("Failed to create .gitignore")?;
        println!("Created .gitignore with .ferrus/ entry");
    }
    Ok(())
}
