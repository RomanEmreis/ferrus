use anyhow::Result;

use crate::project;

pub async fn run() -> Result<()> {
    let report = project::doctor_current_project().await?;
    println!("Project: {}", report.registration.metadata.name);
    println!("Project id: {}", report.registration.local_ref.project_id);
    println!("Data dir: {}", report.registration.data_dir.display());
    println!("Database: {}", report.registration.database_path.display());

    for check in &report.checks {
        let status = if check.ok { "ok" } else { "error" };
        println!("{status}: {}", check.message);
    }

    if report.has_errors() {
        anyhow::bail!("ferrus doctor found project registration issues");
    }
    Ok(())
}
