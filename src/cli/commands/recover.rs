use anyhow::Result;

use crate::project;

pub async fn run() -> Result<()> {
    let recovery = project::recover_runtime_state().await?;
    println!("Interrupted runs: {}", recovery.interrupted_runs);
    println!("Expired task leases: {}", recovery.expired_task_leases);
    println!(
        "STATE lease mirrors cleared: {}",
        recovery.state_lease_mirrors_cleared
    );
    Ok(())
}
