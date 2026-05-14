//! Standalone diagnostic: simulate the seed bundle against mainnet using a
//! payer that matches the multiply role identity (sig_verify=false).
//!
//! Run:
//!   RPC_URL=https://... cargo run --example sim_seed -p multiply-daemon -- <USER_PUBKEY_BS58>

use anyhow::{anyhow, Context, Result};
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::message::{v0::Message as V0Message, VersionedMessage};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::transaction::VersionedTransaction;
use std::str::FromStr;
use std::sync::Arc;
use zerox1_defi_protocols::constants::{
    JITOSOL_MINT, KAMINO_MAIN_JITOSOL_RESERVE, KAMINO_MAIN_MARKET,
};
use zerox1_defi_protocols::protocols::jito::deposit_sol_ix;
use zerox1_defi_protocols::protocols::jito_loader::load_jito_pool;
use zerox1_defi_protocols::protocols::kamino::{
    deposit_ix, derive_user_obligation_with_seed, init_user_metadata_ix,
};
use zerox1_defi_protocols::protocols::kamino_loader::{
    fetch_obligation, load_reserve, user_metadata_exists,
};

const MULTIPLY_OBLIGATION_SEED: (u8, u8) = (0, 1);

#[tokio::main]
async fn main() -> Result<()> {
    let user_str = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow!("usage: sim_seed <USER_PUBKEY_BS58>"))?;
    let user = Pubkey::from_str(&user_str).context("parse user pubkey")?;
    let rpc_url = std::env::var("RPC_URL").context("RPC_URL env")?;
    let rpc = Arc::new(RpcClient::new_with_commitment(
        rpc_url,
        CommitmentConfig::confirmed(),
    ));

    let jitosol_reserve = load_reserve(
        &rpc,
        &KAMINO_MAIN_JITOSOL_RESERVE,
        JITOSOL_MINT,
        &KAMINO_MAIN_MARKET,
    )
    .await?;
    let jito_pool = load_jito_pool(&rpc).await?;

    let obligation_addr = derive_user_obligation_with_seed(
        &user,
        &KAMINO_MAIN_MARKET,
        MULTIPLY_OBLIGATION_SEED.0,
        MULTIPLY_OBLIGATION_SEED.1,
    );
    let decoded = fetch_obligation(&rpc, &obligation_addr).await?;
    let user_metadata_missing = !user_metadata_exists(&rpc, &user).await;
    let obligation_exists = decoded.is_some();

    println!("user                  : {user}");
    println!("obligation_addr       : {obligation_addr}");
    println!("user_metadata_missing : {user_metadata_missing}");
    println!("obligation_exists     : {obligation_exists}");
    println!("jito.withdraw_authority: {}", jito_pool.withdraw_authority);
    println!("jito.reserve_stake     : {}", jito_pool.reserve_stake);
    println!("jito.manager_fee_acct  : {}", jito_pool.manager_fee_account);
    println!("jito.pool_mint         : {}", jito_pool.pool_mint);

    let stake_lamports: u64 = 50_000_000; // 0.05 SOL
    let rate_adjusted = jito_pool.sol_to_jitosol_lamports(stake_lamports);
    let expected_jitosol = rate_adjusted - rate_adjusted / 200;
    println!("\nrate_adjusted_jitosol : {rate_adjusted}");
    println!("expected_jitosol (post haircut): {expected_jitosol}");

    let mut ixs = Vec::new();
    if user_metadata_missing {
        ixs.push(init_user_metadata_ix(&user));
    }
    let jito_ixs = deposit_sol_ix(&user, &jito_pool, stake_lamports)?;
    ixs.extend(jito_ixs);
    let mut deposit_ixs = deposit_ix(
        &user,
        &jitosol_reserve,
        expected_jitosol,
        MULTIPLY_OBLIGATION_SEED,
        &[],
    )?;
    if obligation_exists {
        deposit_ixs.remove(0);
    }
    ixs.extend(deposit_ixs);

    let mut all_ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(1_200_000),
        ComputeBudgetInstruction::set_compute_unit_price(10_000),
    ];
    all_ixs.extend(ixs.clone());

    println!("\nbundle has {} ixs (incl. compute budget):", all_ixs.len());
    for (i, ix) in all_ixs.iter().enumerate() {
        println!(
            "  [{i}] program={} #accts={} data_len={}",
            ix.program_id,
            ix.accounts.len(),
            ix.data.len()
        );
    }

    let recent_blockhash = rpc.get_latest_blockhash().await?;
    let msg = V0Message::try_compile(&user, &all_ixs, &[], recent_blockhash)?;
    let nreq = msg.header.num_required_signatures as usize;
    let tx = VersionedTransaction {
        signatures: vec![Signature::from([0u8; 64]); nreq],
        message: VersionedMessage::V0(msg),
    };

    use solana_rpc_client_api::config::RpcSimulateTransactionConfig;
    let cfg = RpcSimulateTransactionConfig {
        sig_verify: false,
        replace_recent_blockhash: true,
        commitment: Some(CommitmentConfig::confirmed()),
        encoding: None,
        accounts: None,
        min_context_slot: None,
        inner_instructions: false,
    };
    let resp = rpc.simulate_transaction_with_config(&tx, cfg).await?;
    let v = resp.value;
    println!("\nerr: {:?}", v.err);
    println!("units_consumed: {:?}", v.units_consumed);
    println!("logs:");
    if let Some(logs) = v.logs {
        for (i, l) in logs.iter().enumerate() {
            println!("  [{i:02}] {l}");
        }
    }
    Ok(())
}
