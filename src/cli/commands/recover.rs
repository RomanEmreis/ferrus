use anyhow::Result;

use crate::project;

pub async fn run(dry_run: bool) -> Result<()> {
    let recovery = if dry_run {
        project::preview_runtime_recovery().await?
    } else {
        project::recover_runtime_state().await?
    };
    println!(
        "Mode: {}",
        if dry_run {
            "dry-run (no changes)"
        } else {
            "apply"
        }
    );
    println!("Interrupted runs: {}", recovery.interrupted_runs);
    println!("Expired task leases: {}", recovery.expired_task_leases);
    println!(
        "STATE lease mirrors cleared: {}",
        recovery.state_lease_mirrors_cleared
    );
    Ok(())
}
