use anyhow::Result;
use std::time::Duration;
use tracing::debug;

/// Placeholder for batch job consumption. Filled in by the researcher-strategy follow-up plan.
///
/// TODO(strategy plan): add a CancellationToken so the daemon can shut down cleanly on Ctrl-C.
pub async fn run() -> Result<()> {
    loop {
        debug!("researcher idle");
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}
