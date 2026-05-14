//! On-chain loader for the Jito stake pool's `StakePool` account.
//!
//! Decodes the four pubkeys (`reserve_stake`, `manager_fee_account`,
//! `pool_mint`, withdraw_authority via PDA) needed to build a `DepositSol`
//! instruction. Verified against Jito4APyf642JPZPx3hGc6WWJ8zPKtRbRs4P815Awbb
//! on 2026-05-04 — see protocols/jito.rs for the layout.

use anyhow::{bail, Context, Result};
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use zerox1_defi_protocols::{
    constants::{JITOSOL_MINT, JITO_STAKE_POOL},
    protocols::jito::{derive_withdraw_authority, StakePoolMeta},
};

// Field offsets within the SPL StakePool account.
const POOL_RESERVE_STAKE_OFFSET: usize = 130;
const POOL_POOL_MINT_OFFSET: usize = 162;
const POOL_MANAGER_FEE_ACCOUNT_OFFSET: usize = 194;
const POOL_TOTAL_LAMPORTS_OFFSET: usize = 258;
const POOL_POOL_TOKEN_SUPPLY_OFFSET: usize = 266;
const MIN_STAKE_POOL_SIZE: usize = POOL_POOL_TOKEN_SUPPLY_OFFSET + 8;

fn read_pubkey(data: &[u8], offset: usize) -> Pubkey {
    let mut b = [0u8; 32];
    b.copy_from_slice(&data[offset..offset + 32]);
    Pubkey::new_from_array(b)
}

fn read_u64_le(data: &[u8], offset: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&data[offset..offset + 8]);
    u64::from_le_bytes(b)
}

pub async fn load_jito_pool(rpc: &RpcClient) -> Result<StakePoolMeta> {
    let data = rpc
        .get_account_data(&JITO_STAKE_POOL)
        .await
        .with_context(|| format!("fetch Jito stake pool {}", JITO_STAKE_POOL))?;

    if data.len() < MIN_STAKE_POOL_SIZE {
        bail!(
            "Jito stake pool is only {} bytes, expected >= {MIN_STAKE_POOL_SIZE}",
            data.len()
        );
    }

    let reserve_stake = read_pubkey(&data, POOL_RESERVE_STAKE_OFFSET);
    let pool_mint = read_pubkey(&data, POOL_POOL_MINT_OFFSET);
    let manager_fee_account = read_pubkey(&data, POOL_MANAGER_FEE_ACCOUNT_OFFSET);
    let total_lamports = read_u64_le(&data, POOL_TOTAL_LAMPORTS_OFFSET);
    let pool_token_supply = read_u64_le(&data, POOL_POOL_TOKEN_SUPPLY_OFFSET);

    if pool_mint != JITOSOL_MINT {
        bail!(
            "Jito stake pool's pool_mint is {pool_mint}, expected {JITOSOL_MINT} — pool may have been migrated"
        );
    }

    Ok(StakePoolMeta {
        stake_pool: JITO_STAKE_POOL,
        withdraw_authority: derive_withdraw_authority(&JITO_STAKE_POOL),
        reserve_stake,
        manager_fee_account,
        pool_mint,
        total_lamports,
        pool_token_supply,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_pool_buf(reserve: &Pubkey, mint: &Pubkey, fee: &Pubkey) -> Vec<u8> {
        let mut buf = vec![0u8; MIN_STAKE_POOL_SIZE];
        buf[POOL_RESERVE_STAKE_OFFSET..POOL_RESERVE_STAKE_OFFSET + 32]
            .copy_from_slice(&reserve.to_bytes());
        buf[POOL_POOL_MINT_OFFSET..POOL_POOL_MINT_OFFSET + 32].copy_from_slice(&mint.to_bytes());
        buf[POOL_MANAGER_FEE_ACCOUNT_OFFSET..POOL_MANAGER_FEE_ACCOUNT_OFFSET + 32]
            .copy_from_slice(&fee.to_bytes());
        buf
    }

    #[test]
    fn decode_extracts_all_fields() {
        let reserve = Pubkey::new_unique();
        let fee = Pubkey::new_unique();
        let buf = make_pool_buf(&reserve, &JITOSOL_MINT, &fee);
        // Direct field access since `load_jito_pool` requires an RPC client.
        assert_eq!(read_pubkey(&buf, POOL_RESERVE_STAKE_OFFSET), reserve);
        assert_eq!(read_pubkey(&buf, POOL_POOL_MINT_OFFSET), JITOSOL_MINT);
        assert_eq!(read_pubkey(&buf, POOL_MANAGER_FEE_ACCOUNT_OFFSET), fee);
    }
}
