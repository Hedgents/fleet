//! zerox1-defi-cli — drives the local daemon for manual testing.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::json;

#[derive(Parser)]
#[command(name = "zerox1-defi-cli", version, about)]
struct Cli {
    /// Daemon base URL.
    #[arg(long, env = "DAEMON_URL", default_value = "http://127.0.0.1:9091")]
    url: String,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// GET /health
    Health,
    /// GET /identity (wallet pubkey)
    Identity,
    /// POST /kamino/supply
    KaminoSupply {
        /// Asset symbol (currently "usdc" only).
        #[arg(long, default_value = "usdc")]
        asset: String,
        /// Amount in human units (e.g. 1.5). Decimals applied per asset.
        #[arg(long)]
        amount: f64,
        /// Run via simulateTransaction — no broadcast, no SOL spent.
        /// Use this to verify klend account layout before committing real funds.
        #[arg(long, default_value_t = false)]
        simulate: bool,
    },
    /// POST /kamino/withdraw
    KaminoWithdraw {
        #[arg(long, default_value = "usdc")]
        asset: String,
        #[arg(long)]
        amount: f64,
        #[arg(long, default_value_t = false)]
        simulate: bool,
    },
    /// GET /pyth/price/{symbol}
    PythPrice {
        /// Symbol (e.g. SOL, USDC, JITOSOL).
        symbol: String,
    },
}

fn raw_amount(asset: &str, ui_amount: f64) -> Result<u64> {
    let decimals = match asset.to_ascii_lowercase().as_str() {
        "usdc" | "usdt" => 6,
        "sol" | "wsol" => 9,
        "jitosol" | "inf" | "bsol" | "msol" => 9,
        other => anyhow::bail!("unknown asset: {other}"),
    };
    let raw = (ui_amount * 10f64.powi(decimals as i32)).round();
    if raw.is_sign_negative() || !raw.is_finite() {
        anyhow::bail!("invalid amount");
    }
    Ok(raw as u64)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = reqwest::Client::new();

    match cli.cmd {
        Cmd::Health => {
            let r = client
                .get(format!("{}/health", cli.url))
                .send()
                .await?
                .text()
                .await?;
            println!("{r}");
        }
        Cmd::Identity => {
            let r = client
                .get(format!("{}/identity", cli.url))
                .send()
                .await?
                .text()
                .await?;
            println!("{r}");
        }
        Cmd::KaminoSupply {
            asset,
            amount,
            simulate,
        } => {
            let raw = raw_amount(&asset, amount).context("convert amount")?;
            let body = json!({"asset": asset, "amount": raw});
            let url = if simulate {
                format!("{}/kamino/supply?simulate=true", cli.url)
            } else {
                format!("{}/kamino/supply", cli.url)
            };
            let res = client.post(&url).json(&body).send().await?;
            println!("{}", res.text().await?);
        }
        Cmd::KaminoWithdraw {
            asset,
            amount,
            simulate,
        } => {
            let raw = raw_amount(&asset, amount).context("convert amount")?;
            let body = json!({"asset": asset, "amount": raw});
            let url = if simulate {
                format!("{}/kamino/withdraw?simulate=true", cli.url)
            } else {
                format!("{}/kamino/withdraw", cli.url)
            };
            let res = client.post(&url).json(&body).send().await?;
            println!("{}", res.text().await?);
        }
        Cmd::PythPrice { symbol } => {
            let r = client
                .get(format!("{}/pyth/price/{}", cli.url, symbol))
                .send()
                .await?
                .text()
                .await?;
            println!("{r}");
        }
    }
    Ok(())
}
