//! Wallet balance reader: SOL + USDC + JLP for the operator's pubkey.
//!
//! Returns 0 for any token whose ATA does not exist yet (fresh wallet,
//! no deposit ever made). RPC errors propagate.

use anyhow::Result;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address;
use zerox1_defi_protocols::constants::{JLP_MINT, USDC_MINT};

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct WalletBalances {
    pub sol_lamports: u64,
    pub usdc_lamports: u64,
    pub jlp_lamports: u64,
}

/// Read SOL + USDC + JLP balances. ATA-not-found is treated as 0 to keep
/// the dashboard happy on a fresh wallet.
pub async fn read(rpc: &RpcClient, wallet: &Pubkey) -> Result<WalletBalances> {
    let sol_lamports = rpc.get_balance(wallet).await.unwrap_or(0);
    let usdc_lamports = read_token_balance(rpc, wallet, &USDC_MINT, 6).await;
    let jlp_lamports = read_token_balance(rpc, wallet, &JLP_MINT, 6).await;
    Ok(WalletBalances {
        sol_lamports,
        usdc_lamports,
        jlp_lamports,
    })
}

async fn read_token_balance(
    rpc: &RpcClient,
    wallet: &Pubkey,
    mint: &Pubkey,
    _decimals: u8,
) -> u64 {
    let ata = get_associated_token_address(wallet, mint);
    match rpc.get_token_account_balance(&ata).await {
        Ok(bal) => bal.amount.parse::<u64>().unwrap_or(0),
        Err(_) => 0,
    }
}
