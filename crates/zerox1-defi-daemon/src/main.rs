//! zerox1-defi-daemon — localhost HTTP service for Solana DeFi operations.
//!
//! Binds to 127.0.0.1 only by default. Holds wallet keypair. Builds → signs →
//! broadcasts transactions through `zerox1-defi-protocols`. Optionally pairs
//! with a fleet orchestrator via `--fleet-id` + `--fleet-token` + `--role`.
//!
//! Any agent runtime (zeroclaw plugin, Claude Agent SDK script, raw curl) on
//! the same host can call the endpoints.

mod adrena_loader;
mod config;
mod handlers;
mod jito_loader;
mod jlp_loader;
mod kamino_loader;
mod pairing;
mod persistence;
mod rpc;
mod server;
mod wallet;

use std::net::SocketAddr;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::{info, warn};

use zerox1_defi_protocols::constants::{
    JITOSOL_MINT, KAMINO_MAIN_JITOSOL_RESERVE, KAMINO_MAIN_MARKET, KAMINO_MAIN_SOL_RESERVE,
    KAMINO_MAIN_USDC_RESERVE, USDC_MINT, WSOL_MINT,
};

use crate::config::Cli;
use crate::persistence::StateFile;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let cfg = cli.into_config()?;

    let wallet = wallet::Wallet::load(&cfg.wallet_path)?;
    info!(pubkey = %wallet.pubkey(), "loaded wallet");

    let rpc = rpc::RpcContext::with_fallbacks(
        cfg.rpc_url.clone(),
        cfg.fallback_rpc_urls.clone(),
        cfg.commitment,
    );
    info!(
        primary = %cfg.rpc_url,
        fallbacks = cfg.fallback_rpc_urls.len(),
        "connected RPC"
    );

    // Load all three Kamino main-market reserves we touch: USDC (stable
    // floor), SOL (Multiply borrow leg), JitoSOL (Multiply collateral leg).
    let kamino_usdc_reserve = kamino_loader::load_reserve(
        &rpc.client,
        &KAMINO_MAIN_USDC_RESERVE,
        USDC_MINT,
        &KAMINO_MAIN_MARKET,
    )
    .await
    .context("load Kamino USDC reserve metadata")?;
    info!(
        supply_vault = %kamino_usdc_reserve.liquidity_supply,
        fee_vault = %kamino_usdc_reserve.fee_receiver,
        "loaded Kamino USDC reserve"
    );

    let kamino_sol_reserve = kamino_loader::load_reserve(
        &rpc.client,
        &KAMINO_MAIN_SOL_RESERVE,
        WSOL_MINT,
        &KAMINO_MAIN_MARKET,
    )
    .await
    .context("load Kamino SOL reserve metadata")?;
    info!(
        supply_vault = %kamino_sol_reserve.liquidity_supply,
        "loaded Kamino SOL reserve"
    );

    let kamino_jitosol_reserve = kamino_loader::load_reserve(
        &rpc.client,
        &KAMINO_MAIN_JITOSOL_RESERVE,
        JITOSOL_MINT,
        &KAMINO_MAIN_MARKET,
    )
    .await
    .context("load Kamino JitoSOL reserve metadata")?;
    info!(
        supply_vault = %kamino_jitosol_reserve.liquidity_supply,
        "loaded Kamino JitoSOL reserve"
    );

    let jlp_pool = jlp_loader::load_pool(&rpc.client)
        .await
        .context("load JLP pool + custodies")?;
    info!(
        custodies = jlp_pool.custodies.len(),
        perpetuals = %jlp_pool.perpetuals,
        transfer_authority = %jlp_pool.transfer_authority,
        "loaded JLP pool"
    );
    for c in &jlp_pool.custodies {
        info!(
            mint = %c.mint,
            decimals = c.decimals,
            stable = c.is_stable,
            doves = %c.doves_price_account,
            pythnet = %c.pythnet_price_account,
            "  custody"
        );
    }

    let adrena_pool = adrena_loader::load_pool(&rpc.client)
        .await
        .context("load Adrena main-pool + custodies")?;
    info!(
        pool = %adrena_pool.pool,
        cortex = %adrena_pool.cortex,
        oracle = %adrena_pool.oracle,
        jitosol_custody = %adrena_pool.jitosol_custody.address,
        usdc_custody = %adrena_pool.usdc_custody.address,
        "loaded Adrena main-pool"
    );

    let jito_pool = jito_loader::load_jito_pool(&rpc.client)
        .await
        .context("load Jito stake pool metadata")?;
    info!(
        stake_pool = %jito_pool.stake_pool,
        withdraw_authority = %jito_pool.withdraw_authority,
        reserve_stake = %jito_pool.reserve_stake,
        manager_fee_account = %jito_pool.manager_fee_account,
        "loaded Jito stake pool"
    );

    // Promote the partial fleet identity (CLI flags) by attaching the wallet
    // pubkey as the worker's agent_id. Same wallet is used for signing
    // Solana transactions and as the mesh identity.
    let fleet_identity = cfg
        .fleet_identity_partial
        .map(|p| p.complete(wallet.pubkey().to_string()));

    let state_file = StateFile::new(&cfg.data_dir);
    let initial_pairing = if fleet_identity.is_some() {
        match state_file.load() {
            Ok(s) => {
                info!(state = ?s, "loaded pairing state from disk");
                s
            }
            Err(e) => {
                warn!(?e, "could not load pairing state — starting Unpaired");
                pairing::PairingState::Unpaired
            }
        }
    } else {
        pairing::PairingState::Unpaired
    };

    if let Some(id) = &fleet_identity {
        info!(
            fleet_id = %hex::encode(id.fleet_id),
            role = ?id.role,
            topic = %id.discovery_topic(),
            "fleet identity configured"
        );
    } else {
        info!("fleet identity not configured (pairing endpoints will return 503)");
    }

    let state = server::AppState::new(
        rpc,
        wallet,
        fleet_identity,
        initial_pairing,
        state_file,
        kamino_usdc_reserve,
        kamino_sol_reserve,
        kamino_jitosol_reserve,
        jlp_pool,
        adrena_pool,
        jito_pool,
    );

    let addr: SocketAddr = format!("{}:{}", cfg.bind_host, cfg.bind_port).parse()?;
    info!(%addr, "starting daemon");

    let app = server::router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
