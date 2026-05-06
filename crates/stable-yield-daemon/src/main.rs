//! stable-yield-daemon — fleet's passive-supply USDC lender.
//!
//! M1 scaffold: protocol types are defined; CLI, dispatch, and lending
//! ixns land in M3+.

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    tracing::info!("stable-yield-daemon M1 scaffold — see plan for upcoming milestones");
    Ok(())
}
