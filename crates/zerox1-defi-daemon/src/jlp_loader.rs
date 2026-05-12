//! On-chain loader for the JLP pool + 5 custodies.
//!
//! Run once at daemon startup. Fetches the Pool account, decodes the
//! `custodies: Vec<Pubkey>` field, batches a `getMultipleAccounts` for all 5
//! custodies, and decodes `mint`, `token_account`, `pythnet_oracle`,
//! `doves_ag_oracle`, `decimals`, `is_stable` from each at fixed byte offsets.
//!
//! Offsets verified against mainnet on 2026-05-04 — see `protocols/jlp.rs`
//! module docs for the layout.

use anyhow::{bail, Context, Result};
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use zerox1_defi_protocols::{
    constants::{JLP_MINT, JLP_POOL},
    protocols::jlp::{
        derive_event_authority, derive_perpetuals, derive_transfer_authority, CustodyMeta, PoolMeta,
    },
};

// Pool layout (after 8-byte Anchor discriminator):
//   [8..12]  name length (u32 LE)
//   [12..]   name bytes
//   [12+L..] custodies vec: [u32 len][N × 32]
const POOL_NAME_LEN_OFFSET: usize = 8;

// Custody field offsets (after 8-byte Anchor discriminator).
const CUSTODY_MINT_OFFSET: usize = 40;
const CUSTODY_TOKEN_ACCOUNT_OFFSET: usize = 72;
const CUSTODY_DECIMALS_OFFSET: usize = 104;
const CUSTODY_IS_STABLE_OFFSET: usize = 105;
const CUSTODY_PYTHNET_ORACLE_OFFSET: usize = 106; // OracleParams.oracle_account
const CUSTODY_DOVES_AG_ORACLE_OFFSET: usize = 384;
const MIN_CUSTODY_SIZE: usize = CUSTODY_DOVES_AG_ORACLE_OFFSET + 32;

fn read_pubkey(data: &[u8], offset: usize) -> Pubkey {
    let mut b = [0u8; 32];
    b.copy_from_slice(&data[offset..offset + 32]);
    Pubkey::new_from_array(b)
}

fn read_u32_le(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}

/// Decode the `custodies: Vec<Pubkey>` field from a Pool account.
fn decode_pool_custodies(data: &[u8]) -> Result<Vec<Pubkey>> {
    if data.len() < POOL_NAME_LEN_OFFSET + 4 {
        bail!("pool account too small ({} bytes)", data.len());
    }
    let name_len = read_u32_le(data, POOL_NAME_LEN_OFFSET) as usize;
    if name_len > 64 {
        bail!("implausible pool name_len {name_len}");
    }
    let custodies_off = POOL_NAME_LEN_OFFSET + 4 + name_len;
    if data.len() < custodies_off + 4 {
        bail!("pool data truncated before custodies vec");
    }
    let n = read_u32_le(data, custodies_off) as usize;
    if n == 0 || n > 32 {
        bail!("implausible custodies count {n}");
    }
    let mut keys = Vec::with_capacity(n);
    let mut off = custodies_off + 4;
    if data.len() < off + n * 32 {
        bail!(
            "pool data truncated mid-custodies (need {} bytes)",
            off + n * 32
        );
    }
    for _ in 0..n {
        keys.push(read_pubkey(data, off));
        off += 32;
    }
    Ok(keys)
}

fn decode_custody(address: Pubkey, data: &[u8]) -> Result<CustodyMeta> {
    if data.len() < MIN_CUSTODY_SIZE {
        bail!(
            "custody {address} is only {} bytes, expected >= {MIN_CUSTODY_SIZE}",
            data.len()
        );
    }
    Ok(CustodyMeta {
        address,
        mint: read_pubkey(data, CUSTODY_MINT_OFFSET),
        token_account: read_pubkey(data, CUSTODY_TOKEN_ACCOUNT_OFFSET),
        decimals: data[CUSTODY_DECIMALS_OFFSET],
        is_stable: data[CUSTODY_IS_STABLE_OFFSET] != 0,
        pythnet_price_account: read_pubkey(data, CUSTODY_PYTHNET_ORACLE_OFFSET),
        doves_price_account: read_pubkey(data, CUSTODY_DOVES_AG_ORACLE_OFFSET),
    })
}

/// Fetch the JLP `Pool` + all 5 `Custody` accounts and decode them into a
/// `PoolMeta`.
pub async fn load_pool(rpc: &RpcClient) -> Result<PoolMeta> {
    let pool_data = rpc
        .get_account_data(&JLP_POOL)
        .await
        .with_context(|| format!("fetch JLP pool {}", JLP_POOL))?;

    let custody_pubkeys = decode_pool_custodies(&pool_data)?;
    if custody_pubkeys.len() != 5 {
        bail!(
            "expected 5 custodies in JLP pool, decoded {}",
            custody_pubkeys.len()
        );
    }

    let custody_accounts = rpc
        .get_multiple_accounts(&custody_pubkeys)
        .await
        .context("fetch JLP custody accounts")?;

    let mut custodies = Vec::with_capacity(5);
    for (pk, maybe_acct) in custody_pubkeys.iter().zip(custody_accounts.into_iter()) {
        let acct = maybe_acct.with_context(|| format!("custody {pk} not found"))?;
        custodies.push(decode_custody(*pk, &acct.data)?);
    }

    Ok(PoolMeta {
        pool: JLP_POOL,
        jlp_mint: JLP_MINT,
        perpetuals: derive_perpetuals(),
        transfer_authority: derive_transfer_authority(),
        event_authority: derive_event_authority(),
        custodies,
    })
}

// ── Pool AUM + JLP balance helpers (read-only, for risk watcher) ────────────
//
// The Pool account's `aum_usd: u128` field sits at offset
// `12 + name_len + 4 + (custodies_count × 32)` per the Borsh layout. For the
// real JLP pool that's `12 + 4 + 4 + 5*32 = 180` bytes — but we re-decode the
// `name_len` and `custodies_count` defensively in case the pool is ever
// realloc'd to add an asset.

pub async fn fetch_pool_aum_usd(rpc: &RpcClient) -> Result<u128> {
    let data = rpc
        .get_account_data(&JLP_POOL)
        .await
        .with_context(|| format!("fetch JLP pool {} for AUM", JLP_POOL))?;
    decode_aum_usd(&data)
}

fn decode_aum_usd(data: &[u8]) -> Result<u128> {
    if data.len() < 16 {
        bail!("pool data too short for AUM decode");
    }
    let name_len = read_u32_le(data, POOL_NAME_LEN_OFFSET) as usize;
    let custodies_off = POOL_NAME_LEN_OFFSET + 4 + name_len;
    let n = read_u32_le(data, custodies_off) as usize;
    let aum_off = custodies_off + 4 + n * 32;
    if data.len() < aum_off + 16 {
        bail!("pool data truncated before aum_usd");
    }
    Ok(u128::from_le_bytes(
        data[aum_off..aum_off + 16].try_into().unwrap(),
    ))
}

/// Fetch a user's JLP token balance. Returns (raw_amount, decimals=6).
/// Returns `(0, 6)` if the user has no JLP ATA — no JLP held.
pub async fn fetch_user_jlp_balance(rpc: &RpcClient, user_jlp_ata: &Pubkey) -> Result<(u64, u8)> {
    let accounts = rpc
        .get_multiple_accounts(&[*user_jlp_ata])
        .await
        .context("fetch user JLP ATA")?;
    let Some(account) = accounts.into_iter().next().flatten() else {
        return Ok((0, 6));
    };
    if account.data.len() < 72 {
        bail!(
            "JLP ATA {user_jlp_ata} has unexpected size {}",
            account.data.len()
        );
    }
    // SPL Token Account layout: mint(32) + owner(32) + amount(u64) at offset 64..72
    let amount = u64::from_le_bytes(account.data[64..72].try_into().unwrap());
    Ok((amount, 6))
}

/// Fetch the total JLP token supply (from the JLP_MINT's SPL Mint account at
/// offset 36..44 — supply: u64).
pub async fn fetch_jlp_total_supply(rpc: &RpcClient) -> Result<u64> {
    let accounts = rpc
        .get_multiple_accounts(&[JLP_MINT])
        .await
        .context("fetch JLP mint")?;
    let account = accounts
        .into_iter()
        .next()
        .flatten()
        .with_context(|| format!("JLP mint {} not found", JLP_MINT))?;
    if account.data.len() < 44 {
        bail!("JLP mint has unexpected size {}", account.data.len());
    }
    // SPL Mint: mint_authority option(36) + supply(u64) at 36..44
    Ok(u64::from_le_bytes(account.data[36..44].try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_pool_custodies_handles_minimal_pool() {
        // Build a synthetic pool: discriminator(8) + name_len=4 + "Pool"(4) + count=2 + 2 pubkeys
        let mut buf = vec![0u8; 8];
        buf.extend_from_slice(&4u32.to_le_bytes());
        buf.extend_from_slice(b"Pool");
        buf.extend_from_slice(&2u32.to_le_bytes());
        let a = [1u8; 32];
        let b = [2u8; 32];
        buf.extend_from_slice(&a);
        buf.extend_from_slice(&b);

        let custodies = decode_pool_custodies(&buf).expect("decode");
        assert_eq!(custodies.len(), 2);
        assert_eq!(custodies[0].to_bytes(), a);
        assert_eq!(custodies[1].to_bytes(), b);
    }

    #[test]
    fn decode_pool_custodies_rejects_truncated() {
        let buf = vec![0u8; 10];
        assert!(decode_pool_custodies(&buf).is_err());
    }

    #[test]
    fn decode_pool_custodies_rejects_implausible_count() {
        let mut buf = vec![0u8; 8];
        buf.extend_from_slice(&0u32.to_le_bytes()); // empty name
        buf.extend_from_slice(&999u32.to_le_bytes()); // crazy count
        assert!(decode_pool_custodies(&buf).is_err());
    }

    #[test]
    fn decode_custody_reads_all_offsets() {
        let mut buf = vec![0u8; MIN_CUSTODY_SIZE];
        // Fill mint with [11; 32]
        buf[CUSTODY_MINT_OFFSET..CUSTODY_MINT_OFFSET + 32].fill(11);
        buf[CUSTODY_TOKEN_ACCOUNT_OFFSET..CUSTODY_TOKEN_ACCOUNT_OFFSET + 32].fill(22);
        buf[CUSTODY_DECIMALS_OFFSET] = 6;
        buf[CUSTODY_IS_STABLE_OFFSET] = 1;
        buf[CUSTODY_PYTHNET_ORACLE_OFFSET..CUSTODY_PYTHNET_ORACLE_OFFSET + 32].fill(33);
        buf[CUSTODY_DOVES_AG_ORACLE_OFFSET..CUSTODY_DOVES_AG_ORACLE_OFFSET + 32].fill(44);

        let addr = Pubkey::new_unique();
        let c = decode_custody(addr, &buf).expect("decode");
        assert_eq!(c.address, addr);
        assert_eq!(c.mint, Pubkey::new_from_array([11; 32]));
        assert_eq!(c.token_account, Pubkey::new_from_array([22; 32]));
        assert_eq!(c.decimals, 6);
        assert!(c.is_stable);
        assert_eq!(c.pythnet_price_account, Pubkey::new_from_array([33; 32]));
        assert_eq!(c.doves_price_account, Pubkey::new_from_array([44; 32]));
    }

    #[test]
    fn decode_custody_rejects_too_small() {
        let buf = vec![0u8; MIN_CUSTODY_SIZE - 1];
        assert!(decode_custody(Pubkey::new_unique(), &buf).is_err());
    }

    #[test]
    fn decode_aum_finds_value_after_custodies_array() {
        // Build a synthetic pool with name="Pool" + 5 custodies + aum at the
        // expected offset. AUM should sit at 12 + 4 + 4 + 5*32 = 180.
        let mut buf = vec![0u8; 8];
        buf.extend_from_slice(&4u32.to_le_bytes());
        buf.extend_from_slice(b"Pool");
        buf.extend_from_slice(&5u32.to_le_bytes());
        for _ in 0..5 {
            buf.extend_from_slice(&[0u8; 32]);
        }
        // Append aum_usd = 12_345_678_900_000 (u128 LE)
        buf.extend_from_slice(&12_345_678_900_000u128.to_le_bytes());

        let aum = decode_aum_usd(&buf).expect("decode");
        assert_eq!(aum, 12_345_678_900_000);
    }

    #[test]
    fn decode_aum_handles_different_custody_count() {
        // 3-custody pool: AUM at 12 + 4 + 4 + 3*32 = 116
        let mut buf = vec![0u8; 8];
        buf.extend_from_slice(&4u32.to_le_bytes());
        buf.extend_from_slice(b"Test");
        buf.extend_from_slice(&3u32.to_le_bytes());
        for _ in 0..3 {
            buf.extend_from_slice(&[0u8; 32]);
        }
        buf.extend_from_slice(&999u128.to_le_bytes());

        let aum = decode_aum_usd(&buf).expect("decode");
        assert_eq!(aum, 999);
    }
}
