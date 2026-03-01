use anyhow::Result;

pub async fn run() -> Result<()> {
    crate::server::start().await
}
