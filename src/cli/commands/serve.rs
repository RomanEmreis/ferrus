use anyhow::Result;

use crate::server::Role;

pub async fn run(role: Option<Role>) -> Result<()> {
    crate::server::start(role).await
}
