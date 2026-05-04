use anyhow::Result;
use std::time::Duration;
use tracing::debug;

pub async fn run() -> Result<()> {
    loop {
        debug!("researcher idle");
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}
