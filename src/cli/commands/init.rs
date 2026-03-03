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
wait_timeout_secs = 3600 # how long /wait_for_task and /wait_for_review poll before timing out

[agents]
path = ".agents" # root directory for agent skill files
"#;

const SUPERVISOR_SKILL: &str = r#"# Ferrus Supervisor

You are operating as a **Supervisor** in a ferrus-orchestrated project.
Your role is to define tasks and review Executor submissions.

## Starting a new task

1. Call `/create_task` with a detailed Markdown description of what must be done
2. Call `/wait_for_review` — blocks until the Executor submits (safe to call on restart too)
3. Call `/review_pending` to read the full submission context
4. Call `/approve` to accept, or `/reject` with clear and actionable notes
5. Return to step 2 for the next review cycle, or step 1 for a new task

## Resuming after a restart

Call `/wait_for_review` — it returns immediately if a submission is already pending,
otherwise blocks until the Executor submits.

## Notes

- Call `/status` at any time to inspect current state and counters
- Call `/ask_human` if you need clarification from a human
"#;

const EXECUTOR_SKILL: &str = r#"# Ferrus Executor

You are operating as an **Executor** in a ferrus-orchestrated project.
Your role is to implement tasks and iterate until all checks pass and your submission is approved.

## Autonomous loop

1. Call `/wait_for_task` — blocks until a task is assigned (handles restarts and re-addresses after rejection)
2. Read the returned task description, check feedback, and review notes carefully
3. Implement the required changes
4. Call `/check` — fix any failures and repeat until all checks pass
5. Call `/submit` with a summary, manual verification steps, and any known limitations
6. Return to step 1

## Notes

- Check failure details are in `.ferrus/FEEDBACK.md`; full logs are in `.ferrus/logs/`
- Call `/status` at any time to inspect current state and counters
- Call `/ask_human` if you need clarification from a human
"#;

pub async fn run(agents_path: String) -> Result<()> {
    create_ferrus_toml(&agents_path).await?;
    create_ferrus_dir().await?;
    create_skill_files(&agents_path).await?;
    update_gitignore().await?;
    println!("\nferrus initialized. Run `ferrus serve` to start the MCP server.");
    Ok(())
}

async fn create_ferrus_toml(agents_path: &str) -> Result<()> {
    let path = Path::new("ferrus.toml");
    if path.exists() {
        println!("ferrus.toml already exists, skipping.");
    } else {
        // Substitute the agents path into the template
        let content = DEFAULT_FERRUS_TOML.replace(r#"path = ".agents""#, &format!(r#"path = "{agents_path}""#));
        tokio::fs::write(path, content)
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

    for filename in ["TASK.md", "FEEDBACK.md", "REVIEW.md", "SUBMISSION.md", "QUESTION.md", "ANSWER.md"] {
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

async fn create_skill_files(agents_path: &str) -> Result<()> {
    for (role, content) in [
        ("ferrus-supervisor", SUPERVISOR_SKILL),
        ("ferrus-executor", EXECUTOR_SKILL),
    ] {
        let skill_dir = Path::new(agents_path).join("skills").join(role);
        tokio::fs::create_dir_all(&skill_dir)
            .await
            .with_context(|| format!("Failed to create {}", skill_dir.display()))?;

        let skill_path = skill_dir.join("SKILL.md");
        if !skill_path.exists() {
            tokio::fs::write(&skill_path, content)
                .await
                .with_context(|| format!("Failed to write {}", skill_path.display()))?;
            println!("Created {}", skill_path.display());
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
