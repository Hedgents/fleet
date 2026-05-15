//! On-chain state reads with 30s caching.
//!
//! Daemons already poll their own positions via existing telemetry; this
//! module exists so the dashboard's REST endpoints (`/aum`, `/positions`)
//! can render a best-effort live view without coupling to per-daemon
//! JSONL semantics. Reads are cached 30s to keep RPC pressure bounded
//! when the frontend polls every few seconds.

use anyhow::Result;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;

pub mod balance;
pub mod jlp_price;
pub mod jupiter_perps;
pub mod kamino;
pub mod rates;

const CACHE_TTL: Duration = Duration::from_secs(30);
const RATES_CACHE_TTL: Duration = Duration::from_secs(300); // 5 min — rates move slowly

pub struct ChainReader {
    pub rpc: Arc<RpcClient>,
    cache: RwLock<ChainCache>,
}

#[derive(Default)]
struct ChainCache {
    wallet_balances: Option<(Instant, balance::WalletBalances)>,
    multiply_position: Option<(Instant, Option<kamino::ObligationView>)>,
    stable_yield_position: Option<(Instant, Option<kamino::SupplyView>)>,
    hedgedjlp_position: Option<(Instant, jupiter_perps::PositionView)>,
    rate_snapshot: Option<(Instant, rates::RateSnapshot)>,
}

impl ChainReader {
    pub fn new(rpc_url: String) -> Self {
        Self {
            rpc: Arc::new(RpcClient::new(rpc_url)),
            cache: RwLock::new(ChainCache::default()),
        }
    }

    /// Read SOL/USDC/JLP balances for the operator wallet, cache 30s.
    pub async fn wallet_balances(&self, wallet: &Pubkey) -> Result<balance::WalletBalances> {
        if let Some((ts, val)) = &self.cache.read().await.wallet_balances {
            if ts.elapsed() < CACHE_TTL {
                return Ok(val.clone());
            }
        }
        let fresh = balance::read(&self.rpc, wallet).await?;
        let mut g = self.cache.write().await;
        g.wallet_balances = Some((Instant::now(), fresh.clone()));
        Ok(fresh)
    }

    /// Read multiply's obligation, cache 30s.
    pub async fn multiply_position(
        &self,
        wallet: &Pubkey,
        market: &Pubkey,
    ) -> Result<Option<kamino::ObligationView>> {
        if let Some((ts, val)) = &self.cache.read().await.multiply_position {
            if ts.elapsed() < CACHE_TTL {
                return Ok(val.clone());
            }
        }
        let fresh = kamino::read_multiply_obligation(&self.rpc, wallet, market).await?;
        let mut g = self.cache.write().await;
        g.multiply_position = Some((Instant::now(), fresh.clone()));
        Ok(fresh)
    }

    /// Read stable-yield's supply view, cache 30s.
    pub async fn stable_yield_position(
        &self,
        wallet: &Pubkey,
        market: &Pubkey,
        reserve: &Pubkey,
    ) -> Result<Option<kamino::SupplyView>> {
        if let Some((ts, val)) = &self.cache.read().await.stable_yield_position {
            if ts.elapsed() < CACHE_TTL {
                return Ok(val.clone());
            }
        }
        let fresh = kamino::read_stable_yield_supply(&self.rpc, wallet, market, reserve).await?;
        let mut g = self.cache.write().await;
        g.stable_yield_position = Some((Instant::now(), fresh.clone()));
        Ok(fresh)
    }

    /// Fetch yield benchmark rates, cache 5 min.
    pub async fn rate_snapshot(&self) -> rates::RateSnapshot {
        if let Some((ts, snap)) = &self.cache.read().await.rate_snapshot {
            if ts.elapsed() < RATES_CACHE_TTL {
                return snap.clone();
            }
        }
        let (bps, note) = rates::fetch_kamino_usdc_apy().await;
        let fresh = rates::RateSnapshot {
            kamino_usdc_supply_bps: bps,
            kamino_note: note,
            kamino_fetched_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            ..Default::default()
        };
        let mut g = self.cache.write().await;
        g.rate_snapshot = Some((Instant::now(), fresh.clone()));
        fresh
    }

    /// Read hedgedjlp's position view, cache 30s.
    pub async fn hedgedjlp_position(&self, wallet: &Pubkey) -> Result<jupiter_perps::PositionView> {
        if let Some((ts, val)) = &self.cache.read().await.hedgedjlp_position {
            if ts.elapsed() < CACHE_TTL {
                return Ok(val.clone());
            }
        }
        let fresh = jupiter_perps::read_jupiter_perps_position(&self.rpc, wallet).await?;
        let mut g = self.cache.write().await;
        g.hedgedjlp_position = Some((Instant::now(), fresh.clone()));
        Ok(fresh)
    }
}
