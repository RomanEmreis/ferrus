use anyhow::Result;
use neva::prelude::*;

mod tools;

pub async fn start() -> Result<()> {
    App::new()
        .with_options(|opt| {
            opt.with_stdio()
                .with_name("ferrus")
                .with_version(env!("CARGO_PKG_VERSION"))
        })
        .run()
        .await;
    Ok(())
}
