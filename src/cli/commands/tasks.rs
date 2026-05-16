use anyhow::Result;
use clap::Subcommand;

use crate::project;

#[derive(Debug, Subcommand)]
pub enum TasksCommand {
    /// List task rows from ferrus.db.
    List,
}

pub async fn run(command: TasksCommand) -> Result<()> {
    match command {
        TasksCommand::List => list().await,
    }
}

async fn list() -> Result<()> {
    let tasks = project::list_tasks().await?;
    if tasks.is_empty() {
        println!("No tasks recorded.");
        return Ok(());
    }

    println!(
        "{:<14} {:<14} {:<24} {:<22} {:<22} Path",
        "ID", "Status", "Claimed by", "Lease until", "Heartbeat"
    );
    for task in tasks {
        println!(
            "{:<14} {:<14} {:<24} {:<22} {:<22} {}",
            task.id,
            task.status,
            task.claimed_by.as_deref().unwrap_or("-"),
            task.lease_until.as_deref().unwrap_or("-"),
            task.last_heartbeat.as_deref().unwrap_or("-"),
            task.path,
        );
    }
    Ok(())
}
