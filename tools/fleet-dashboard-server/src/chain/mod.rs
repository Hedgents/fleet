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

use async_trait::async_trait;
use solana_rpc_client::{
    http_sender::HttpSender,
    nonblocking::rpc_client::RpcClient,
    rpc_client::RpcClientConfig,
    rpc_sender::{RpcSender, RpcTransportStats},
};
use solana_rpc_client_api::{client_error::Result as ClientResult, request::RpcRequest};
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;

/// Public Solana RPC failover (overridable via `FALLBACK_RPC_URL`). Mirrors
/// `zerox1_defi_runtime::rpc::FailoverSender`; replicated here so the
/// dashboard stays off the heavy runtime crate. When the primary (e.g. a
/// 429'd Helius key) fails, reads fall over to the public RPC so the
/// dashboard never renders a blind $0 again.
const DEFAULT_FALLBACK_RPC: &str = "https://api.mainnet-beta.solana.com";

struct FailoverSender {
    primary: HttpSender,
    fallback: HttpSender,
    primary_url: String,
}

#[async_trait]
impl RpcSender for FailoverSender {
    async fn send(
        &self,
        request: RpcRequest,
        params: serde_json::Value,
    ) -> ClientResult<serde_json::Value> {
        match self.primary.send(request, params.clone()).await {
            Ok(v) => Ok(v),
            Err(e) => {
                tracing::warn!(?request, error = %e, "primary RPC failed — failing over to public RPC");
                self.fallback.send(request, params).await
            }
        }
    }
    fn get_transport_stats(&self) -> RpcTransportStats {
        self.primary.get_transport_stats()
    }
    fn url(&self) -> String {
        self.primary_url.clone()
    }
}

fn failover_client(rpc_url: String) -> RpcClient {
    let fallback =
        std::env::var("FALLBACK_RPC_URL").unwrap_or_else(|_| DEFAULT_FALLBACK_RPC.to_string());
    let sender = FailoverSender {
        primary: HttpSender::new(rpc_url.clone()),
        fallback: HttpSender::new(fallback),
        primary_url: rpc_url,
    };
    RpcClient::new_sender(
        sender,
        RpcClientConfig::with_commitment(CommitmentConfig::confirmed()),
    )
}

pub mod balance;
pub mod jlp_price;
pub mod jupiter_perps;
pub mod kamino;
pub mod rates;
pub mod sol_price;

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
    onyc_position: Option<(Instant, Option<kamino::ObligationView>)>,
    stable_yield_position: Option<(Instant, Option<kamino::SupplyView>)>,
    hedgedjlp_position: Option<(Instant, jupiter_perps::PositionView)>,
    rate_snapshot: Option<(Instant, rates::RateSnapshot)>,
    /// v0.4.12: SOL/USD mark from Jupiter Lite Price API. Cached 30s,
    /// same TTL as the JLP price. Powers idle-SOL-in-AUM accounting.
    /// `None` until first fetch; `Some((ts, 0))` on a failed fetch so
    /// the cache TTL paces retries rather than thrashing on every
    /// dashboard tick.
    sol_price_micro_usd: Option<(Instant, u128)>,
}

impl ChainReader {
    pub fn new(rpc_url: String) -> Self {
        Self {
            rpc: Arc::new(failover_client(rpc_url)),
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

    /// Read onyc's obligation, cache 30s. ONyc lives in Kamino's
    /// isolated ONyc market (separate market from main); obligation
    /// seed is (0, 2) — distinct from stable-yield (0,0) and multiply (0,1).
    pub async fn onyc_position(
        &self,
        wallet: &Pubkey,
        market: &Pubkey,
    ) -> Result<Option<kamino::ObligationView>> {
        if let Some((ts, val)) = &self.cache.read().await.onyc_position {
            if ts.elapsed() < CACHE_TTL {
                return Ok(val.clone());
            }
        }
        let fresh = kamino::read_onyc_obligation(&self.rpc, wallet, market).await?;
        let mut g = self.cache.write().await;
        g.onyc_position = Some((Instant::now(), fresh.clone()));
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

    /// v0.4.12: SOL/USD price from Jupiter Lite Price API, cache 30s.
    /// Returns 0 (and caches the zero) on fetch failure so callers can
    /// degrade gracefully — a zero price means the SOL component of
    /// idle wallet capital silently rolls to zero this cycle and the
    /// next fetch happens in 30s. Never panics, never blocks the AUM
    /// response.
    pub async fn sol_price_micro_usd(&self) -> u128 {
        if let Some((ts, price)) = &self.cache.read().await.sol_price_micro_usd {
            if ts.elapsed() < CACHE_TTL {
                return *price;
            }
        }
        let fresh = sol_price::fetch_sol_price_micro_usd().await.unwrap_or(0);
        let mut g = self.cache.write().await;
        g.sol_price_micro_usd = Some((Instant::now(), fresh));
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
