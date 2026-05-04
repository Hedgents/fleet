use anyhow::Result;
use std::time::Duration;
use tracing::debug;

/// Placeholder for Pyth pull + Yellowstone gRPC subscriptions.
/// Filled in by the riskwatcher-strategy follow-up plan.
pub async fn run() -> Result<()> {
    loop {
        debug!("riskwatcher tick");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
