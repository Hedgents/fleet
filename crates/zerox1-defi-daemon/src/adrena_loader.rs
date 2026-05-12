//! On-chain loader for Adrena's main pool + JitoSOL/USDC custodies.
//!
//! Fetches the JitoSOL and USDC `Custody` accounts (we don't need the
//! pool struct itself at runtime — the pool address is constant), decodes
//! the mint, token_account, decimals, and is_stable fields at fixed
//! byte offsets, and packages everything into an
//! `adrena::PoolMeta` with all the program-level PDAs pre-derived.
//!
//! Custody offsets (verified against mainnet on 2026-05-04, layout from
//! Adrena IDL v1.4.0):
//!
//! ```text
//! [ 0..8]   Anchor discriminator
//! [ 8]      bump
//! [ 9]      token_account_bump
//! [10]      allow_trade
//! [11]      allow_swap
//! [12]      decimals
//! [13]      is_stable
//! [14..16]  padding
//! [16..48]  pool
//! [48..80]  mint
//! [80..112] token_account
//! ...       (oracle, pricing, fees, ... — not needed for instruction building)
//! ```

use anyhow::{bail, Context, Result};
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use zerox1_defi_protocols::{
    constants::{ADRENA_CUSTODY_JITOSOL, ADRENA_CUSTODY_USDC, ADRENA_MAIN_POOL},
    protocols::adrena::{
        derive_cortex, derive_oracle, derive_transfer_authority, CustodyMeta, PoolMeta,
    },
};

const CUSTODY_DECIMALS_OFFSET: usize = 12;
const CUSTODY_IS_STABLE_OFFSET: usize = 13;
const CUSTODY_POOL_OFFSET: usize = 16;
const CUSTODY_MINT_OFFSET: usize = 48;
const CUSTODY_TOKEN_ACCOUNT_OFFSET: usize = 80;
const MIN_CUSTODY_SIZE: usize = CUSTODY_TOKEN_ACCOUNT_OFFSET + 32;

fn read_pubkey(data: &[u8], offset: usize) -> Pubkey {
    let mut b = [0u8; 32];
    b.copy_from_slice(&data[offset..offset + 32]);
    Pubkey::new_from_array(b)
}

fn decode_custody(address: Pubkey, data: &[u8], expected_pool: &Pubkey) -> Result<CustodyMeta> {
    if data.len() < MIN_CUSTODY_SIZE {
        bail!(
            "custody {address} is only {} bytes, expected >= {MIN_CUSTODY_SIZE}",
            data.len()
        );
    }
    let pool = read_pubkey(data, CUSTODY_POOL_OFFSET);
    if &pool != expected_pool {
        bail!("custody {address} belongs to pool {pool}, expected {expected_pool}");
    }
    Ok(CustodyMeta {
        address,
        mint: read_pubkey(data, CUSTODY_MINT_OFFSET),
        token_account: read_pubkey(data, CUSTODY_TOKEN_ACCOUNT_OFFSET),
        decimals: data[CUSTODY_DECIMALS_OFFSET],
        is_stable: data[CUSTODY_IS_STABLE_OFFSET] != 0,
    })
}

/// Fetch JitoSOL + USDC custody accounts in a single batch call and decode
/// them. Returns a `PoolMeta` with all PDAs pre-derived for later use.
pub async fn load_pool(rpc: &RpcClient) -> Result<PoolMeta> {
    let custody_keys = [ADRENA_CUSTODY_JITOSOL, ADRENA_CUSTODY_USDC];
    let accounts = rpc
        .get_multiple_accounts(&custody_keys)
        .await
        .context("fetch Adrena custodies")?;

    let jitosol_acct = accounts[0].as_ref().with_context(|| {
        format!(
            "Adrena JitoSOL custody {} not found",
            ADRENA_CUSTODY_JITOSOL
        )
    })?;
    let usdc_acct = accounts[1]
        .as_ref()
        .with_context(|| format!("Adrena USDC custody {} not found", ADRENA_CUSTODY_USDC))?;

    let jitosol_custody = decode_custody(
        ADRENA_CUSTODY_JITOSOL,
        &jitosol_acct.data,
        &ADRENA_MAIN_POOL,
    )?;
    let usdc_custody = decode_custody(ADRENA_CUSTODY_USDC, &usdc_acct.data, &ADRENA_MAIN_POOL)?;

    if !usdc_custody.is_stable {
        bail!("Adrena USDC custody is not marked stable — pool layout may have changed");
    }
    if jitosol_custody.is_stable {
        bail!("Adrena JitoSOL custody is marked stable — wrong custody address?");
    }

    Ok(PoolMeta {
        pool: ADRENA_MAIN_POOL,
        cortex: derive_cortex(),
        transfer_authority: derive_transfer_authority(),
        oracle: derive_oracle(),
        jitosol_custody,
        usdc_custody,
    })
}

// ── Position decoding ───────────────────────────────────────────────────────
//
// Position account layout (Borsh, after 8-byte Anchor discriminator):
//
// ```text
// [  0..8]    discriminator
// [  8]      bump (u8)
// [  9]      side (u8)              1=Long, 2=Short
// [ 10]      take_profit_is_set (u8)
// [ 11]      stop_loss_is_set (u8)
// [ 12]      padding_unsafe[1]      (u8)
// [ 13..16]  padding[3]             (u8;3)
// [ 16..48]  owner (Pubkey)
// [ 48..80]  pool (Pubkey)
// [ 80..112] custody (Pubkey)
// [112..144] collateral_custody (Pubkey)
// [144..152] open_time (i64)
// [152..160] update_time (i64)
// [160..168] price (u64)
// [168..176] size_usd (u64)
// [176..184] borrow_size_usd (u64)
// [184..192] collateral_usd (u64)   collateral value in USD (6 decimals)
// [192..200] unrealized_interest_usd (u64)
// [200..216] cumulative_interest_snapshot (U128Split = 16 bytes)
// [216..224] locked_amount (u64)
// [224..232] collateral_amount (u64) collateral in raw token units
// [232..240] exit_fee_usd (u64)
// [240..248] liquidation_fee_usd (u64)
// [248..256] id (u64)
// ...
// ```

const POSITION_DISCRIMINATOR: [u8; 8] = [0xaa, 0xbc, 0x8f, 0xe4, 0x7a, 0x40, 0xf7, 0xd0];

const POSITION_MIN_SIZE: usize = 248;

#[derive(Debug, Clone, serde::Serialize)]
pub struct DecodedPosition {
    pub address: Pubkey,
    pub owner: Pubkey,
    pub pool: Pubkey,
    pub custody: Pubkey,
    pub collateral_custody: Pubkey,
    pub side: u8,
    pub open_time: i64,
    pub update_time: i64,
    /// Entry price in 6-decimal USD scaling.
    pub entry_price_usd_e6: u64,
    /// Position size in 6-decimal USD scaling.
    pub size_usd_e6: u64,
    /// Borrowed amount in 6-decimal USD scaling.
    pub borrow_size_usd_e6: u64,
    /// Collateral USD value, 6-decimal scaling.
    pub collateral_usd_e6: u64,
    /// Collateral in raw token units of the collateral mint.
    pub collateral_amount: u64,
    /// Unrealized borrow interest accrued so far.
    pub unrealized_interest_usd_e6: u64,
}

pub fn decode_position(address: Pubkey, data: &[u8]) -> Result<DecodedPosition> {
    if data.len() < POSITION_MIN_SIZE {
        bail!(
            "position {address} is only {} bytes, expected >= {POSITION_MIN_SIZE}",
            data.len()
        );
    }
    if data[0..8] != POSITION_DISCRIMINATOR {
        bail!(
            "account {address} is not an Adrena Position (disc {:?})",
            &data[0..8]
        );
    }
    let side = data[9];
    let owner = read_pubkey(data, 16);
    let pool = read_pubkey(data, 48);
    let custody = read_pubkey(data, 80);
    let collateral_custody = read_pubkey(data, 112);
    let open_time = i64::from_le_bytes(data[144..152].try_into().unwrap());
    let update_time = i64::from_le_bytes(data[152..160].try_into().unwrap());
    let entry_price = u64::from_le_bytes(data[160..168].try_into().unwrap());
    let size_usd = u64::from_le_bytes(data[168..176].try_into().unwrap());
    let borrow_size_usd = u64::from_le_bytes(data[176..184].try_into().unwrap());
    let collateral_usd = u64::from_le_bytes(data[184..192].try_into().unwrap());
    let unrealized_interest = u64::from_le_bytes(data[192..200].try_into().unwrap());
    let collateral_amount = u64::from_le_bytes(data[224..232].try_into().unwrap());

    Ok(DecodedPosition {
        address,
        owner,
        pool,
        custody,
        collateral_custody,
        side,
        open_time,
        update_time,
        entry_price_usd_e6: entry_price,
        size_usd_e6: size_usd,
        borrow_size_usd_e6: borrow_size_usd,
        collateral_usd_e6: collateral_usd,
        collateral_amount,
        unrealized_interest_usd_e6: unrealized_interest,
    })
}

/// Fetch and decode an Adrena Position account. Returns `Ok(None)` if the
/// account doesn't exist on chain (user has no open position for this side).
pub async fn fetch_position(rpc: &RpcClient, position: &Pubkey) -> Result<Option<DecodedPosition>> {
    let accounts = rpc
        .get_multiple_accounts(&[*position])
        .await
        .with_context(|| format!("fetch position {position}"))?;
    let Some(account) = accounts.into_iter().next().flatten() else {
        return Ok(None);
    };
    Ok(Some(decode_position(*position, &account.data)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_custody_buf(pool: &Pubkey, mint: &Pubkey, decimals: u8, stable: bool) -> Vec<u8> {
        let mut buf = vec![0u8; MIN_CUSTODY_SIZE];
        buf[CUSTODY_DECIMALS_OFFSET] = decimals;
        buf[CUSTODY_IS_STABLE_OFFSET] = if stable { 1 } else { 0 };
        buf[CUSTODY_POOL_OFFSET..CUSTODY_POOL_OFFSET + 32].copy_from_slice(&pool.to_bytes());
        buf[CUSTODY_MINT_OFFSET..CUSTODY_MINT_OFFSET + 32].copy_from_slice(&mint.to_bytes());
        buf
    }

    #[test]
    fn decode_custody_reads_all_fields() {
        let pool = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let buf = make_custody_buf(&pool, &mint, 6, true);
        let addr = Pubkey::new_unique();
        let c = decode_custody(addr, &buf, &pool).expect("decode");
        assert_eq!(c.address, addr);
        assert_eq!(c.mint, mint);
        assert_eq!(c.decimals, 6);
        assert!(c.is_stable);
    }

    #[test]
    fn decode_custody_rejects_wrong_pool() {
        let pool = Pubkey::new_unique();
        let other = Pubkey::new_unique();
        let buf = make_custody_buf(&pool, &Pubkey::new_unique(), 6, true);
        assert!(decode_custody(Pubkey::new_unique(), &buf, &other).is_err());
    }

    #[test]
    fn decode_custody_rejects_too_small() {
        let buf = vec![0u8; MIN_CUSTODY_SIZE - 1];
        assert!(decode_custody(Pubkey::new_unique(), &buf, &Pubkey::default()).is_err());
    }

    // ── Position decoder tests ──────────────────────────────────────────────

    fn make_position_buf(
        owner: &Pubkey,
        pool: &Pubkey,
        custody: &Pubkey,
        collat: &Pubkey,
        side: u8,
        size_usd: u64,
        collateral_usd: u64,
        collateral_amt: u64,
    ) -> Vec<u8> {
        let mut buf = vec![0u8; POSITION_MIN_SIZE];
        buf[0..8].copy_from_slice(&POSITION_DISCRIMINATOR);
        buf[9] = side;
        buf[16..48].copy_from_slice(&owner.to_bytes());
        buf[48..80].copy_from_slice(&pool.to_bytes());
        buf[80..112].copy_from_slice(&custody.to_bytes());
        buf[112..144].copy_from_slice(&collat.to_bytes());
        buf[168..176].copy_from_slice(&size_usd.to_le_bytes());
        buf[184..192].copy_from_slice(&collateral_usd.to_le_bytes());
        buf[224..232].copy_from_slice(&collateral_amt.to_le_bytes());
        buf
    }

    #[test]
    fn decode_position_extracts_all_pubkeys() {
        let owner = Pubkey::new_unique();
        let pool = Pubkey::new_unique();
        let custody = Pubkey::new_unique();
        let collat = Pubkey::new_unique();
        let buf = make_position_buf(&owner, &pool, &custody, &collat, 2, 100, 50, 50_000_000);
        let addr = Pubkey::new_unique();
        let p = decode_position(addr, &buf).expect("decode");
        assert_eq!(p.address, addr);
        assert_eq!(p.owner, owner);
        assert_eq!(p.pool, pool);
        assert_eq!(p.custody, custody);
        assert_eq!(p.collateral_custody, collat);
        assert_eq!(p.side, 2);
    }

    #[test]
    fn decode_position_extracts_size_and_collateral() {
        let buf = make_position_buf(
            &Pubkey::default(),
            &Pubkey::default(),
            &Pubkey::default(),
            &Pubkey::default(),
            2,
            500_000_000,
            100_000_000,
            100_000_000,
        );
        let p = decode_position(Pubkey::new_unique(), &buf).expect("decode");
        assert_eq!(p.size_usd_e6, 500_000_000);
        assert_eq!(p.collateral_usd_e6, 100_000_000);
        assert_eq!(p.collateral_amount, 100_000_000);
    }

    #[test]
    fn decode_position_rejects_wrong_disc() {
        let mut buf = vec![0u8; POSITION_MIN_SIZE];
        buf[0] = 0xff;
        assert!(decode_position(Pubkey::new_unique(), &buf).is_err());
    }

    #[test]
    fn decode_position_rejects_too_small() {
        let mut buf = vec![0u8; 100];
        buf[0..8].copy_from_slice(&POSITION_DISCRIMINATOR);
        assert!(decode_position(Pubkey::new_unique(), &buf).is_err());
    }
}
