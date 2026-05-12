//! On-chain loader for Kamino Reserve and Obligation accounts.
//!
//! Fetches the raw Reserve account from the RPC and decodes the four pubkeys
//! needed to build deposit/withdraw instructions by fixed byte offsets. The
//! offsets are stable for the klend version currently deployed at
//! KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD.
//!
//! Verified against mainnet reserve D6q6wuQSrifJKZYpR1M8R4YawnLDtDsMmWM1NbBmgJ59
//! on 2026-05-04 (account size 8624 bytes, discriminator 2bf2ccca1af73b7f).

use crate::protocols::kamino::ReserveAccounts;
use anyhow::{bail, Context, Result};
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;

// ── Byte offsets within the klend Reserve account ───────────────────────────
//
// Reserve layout (Anchor / Borsh):
//   [0..8]    discriminator
//   [8..16]   version: u64
//   [16..32]  last_update: LastUpdate { slot: u64, stale: u8, _pad: [u8;7] }
//   [32..64]  lending_market: Pubkey
//   [64..96]  farm_collateral: Pubkey
//   [96..128] farm_debt: Pubkey
//   [128..]   ReserveLiquidity (1232 bytes) + reserve_liquidity_padding [u64;150]
//   [2560..]  ReserveCollateral: mint_pubkey(32) + mint_total_supply(u64) + supply_vault(32)
//   ...

const LENDING_MARKET_OFFSET: usize = 32;
const FARM_COLLATERAL_OFFSET: usize = 64;
const LIQUIDITY_SUPPLY_VAULT_OFFSET: usize = 160;
const LIQUIDITY_FEE_VAULT_OFFSET: usize = 192;
const COLLATERAL_MINT_OFFSET: usize = 2560;
const COLLATERAL_SUPPLY_VAULT_OFFSET: usize = 2600; // mint_pubkey(32) + mint_total_supply(u64)
                                                    // Scope oracle pubkey is stored at this offset in the Reserve config section.
                                                    // Verified against mainnet USDC reserve D6q6wuQSrifJKZYpR1M8R4YawnLDtDsMmWM1NbBmgJ59
                                                    // on 2026-05-04. Value at this offset: 3t4JZcueEzTbVP6kLxXrL3VpWx45jDer4eqysweBchNH
const SCOPE_ORACLE_OFFSET: usize = 5112;

// Expected Anchor discriminator for the Reserve account type.
// sha256("account:Reserve")[0..8]
const RESERVE_DISCRIMINATOR: [u8; 8] = [0x2b, 0xf2, 0xcc, 0xca, 0x1a, 0xf7, 0x3b, 0x7f];

// Minimum account size needed to read all fields above.
// Must cover at least SCOPE_ORACLE_OFFSET + 32 = 5144.
const MIN_RESERVE_SIZE: usize = SCOPE_ORACLE_OFFSET + 32;

fn read_pubkey(data: &[u8], offset: usize) -> Pubkey {
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&data[offset..offset + 32]);
    Pubkey::new_from_array(bytes)
}

// ── Obligation decoding ─────────────────────────────────────────────────────
//
// Obligation account layout (Borsh, after 8-byte Anchor discriminator):
//
// ```text
// [   0..8]    discriminator
// [   8..16]  tag (u64)
// [  16..32]  last_update (slot u64 + stale u8 + price_status u8 + placeholder [u8;6])
// [  32..64]  lending_market (Pubkey)
// [  64..96]  owner (Pubkey)
// [  96..1184] deposits: [ObligationCollateral; 8]   (8 × 136 bytes)
// [1184..1192] lowest_reserve_deposit_liquidation_ltv (u64)
// [1192..1208] deposited_value_sf (u128, sf-scaled)
// [1208..2128] borrows: [ObligationLiquidity; 5]     (5 × 184 bytes)
// [2128..2144] borrow_factor_adjusted_debt_value_sf (u128, sf-scaled)
// [2144..2160] borrowed_assets_market_value_sf (u128, sf-scaled)
// [2160..2176] allowed_borrow_value_sf (u128, sf-scaled)
// [2176..2192] unhealthy_borrow_value_sf (u128, sf-scaled)
// ...  (deposits_asset_tiers, borrows_asset_tiers, flags, referrer, padding)
// ```
//
// SF (scaled fraction) values: divide by 2^60 to get the real number. For
// market_value_sf fields the result is in USD; for borrowed_amount_sf it's in
// the liability mint's token units.
//
// ObligationCollateral (136 bytes):  reserve(32) + deposited_amount(u64) +
// market_value_sf(u128) + borrowed_amount_against_this_collateral_in_eg(u64) +
// padding[u64;9].
//
// ObligationLiquidity (184 bytes): reserve(32) + cumulative_borrow_rate_bsf(16) +
// padding(u64) + borrowed_amount_sf(u128) + market_value_sf(u128) +
// borrow_factor_adjusted_market_value_sf(u128) +
// borrowed_amount_outside_eg(u64) + borrowed_amounts_in_eg[u64;8] + padding(u64).

/// Anchor `account:Obligation` discriminator (sha256("account:Obligation")[..8]).
const OBLIGATION_DISCRIMINATOR: [u8; 8] = [0xa8, 0xce, 0x8d, 0x6a, 0x58, 0x4c, 0xac, 0xa7];

const OBLIGATION_LENDING_MARKET_OFFSET: usize = 32;
const OBLIGATION_OWNER_OFFSET: usize = 64;
const OBLIGATION_DEPOSITS_OFFSET: usize = 96;
const OBLIGATION_DEPOSIT_SLOT_SIZE: usize = 136;
const OBLIGATION_DEPOSIT_SLOTS: usize = 8;
const OBLIGATION_BORROWS_OFFSET: usize = 1208;
const OBLIGATION_BORROW_SLOT_SIZE: usize = 184;
const OBLIGATION_BORROW_SLOTS: usize = 5;
const OBLIGATION_DEPOSITED_VALUE_OFFSET: usize = 1192;
const OBLIGATION_AGGREGATE_OFFSET: usize = 2128;
const OBLIGATION_MIN_SIZE: usize = OBLIGATION_AGGREGATE_OFFSET + 16 * 4;

#[derive(Debug, Clone, serde::Serialize)]
pub struct ObligationDeposit {
    /// The reserve this deposit is against.
    pub reserve: Pubkey,
    /// Amount of cTokens (collateral tokens) deposited, raw units.
    pub deposited_amount: u64,
    /// USD value, sf-scaled (divide by 2^60 for the dollar amount).
    pub market_value_sf: u128,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ObligationBorrow {
    /// The reserve being borrowed from.
    pub reserve: Pubkey,
    /// Borrowed amount in liability units, sf-scaled (divide by 2^60).
    pub borrowed_amount_sf: u128,
    /// USD value of the debt, sf-scaled.
    pub market_value_sf: u128,
    /// Borrow-factor-adjusted USD value (used for unhealthy LTV math).
    pub borrow_factor_adjusted_market_value_sf: u128,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DecodedObligation {
    pub address: Pubkey,
    pub lending_market: Pubkey,
    pub owner: Pubkey,
    pub deposits: Vec<ObligationDeposit>,
    pub borrows: Vec<ObligationBorrow>,
    /// Total deposited collateral USD, sf-scaled.
    pub deposited_value_sf: u128,
    /// Total debt USD, borrow-factor-adjusted, sf-scaled.
    pub borrow_factor_adjusted_debt_value_sf: u128,
    /// Total debt USD without borrow-factor adjustment, sf-scaled.
    pub borrowed_assets_market_value_sf: u128,
    /// Max debt USD allowed by current LTVs, sf-scaled.
    pub allowed_borrow_value_sf: u128,
    /// Liquidation threshold USD, sf-scaled. Health factor breaches when
    /// `borrow_factor_adjusted_debt_value_sf >= unhealthy_borrow_value_sf`.
    pub unhealthy_borrow_value_sf: u128,
}

/// Decode an Obligation account from raw bytes. Returns `None` if the
/// discriminator doesn't match (caller passes a wrong account).
pub fn decode_obligation(address: Pubkey, data: &[u8]) -> Result<DecodedObligation> {
    if data.len() < OBLIGATION_MIN_SIZE {
        bail!(
            "obligation {address} is only {} bytes, expected >= {OBLIGATION_MIN_SIZE}",
            data.len()
        );
    }
    if data[0..8] != OBLIGATION_DISCRIMINATOR {
        bail!(
            "account {address} is not a Kamino Obligation (disc {:?})",
            &data[0..8]
        );
    }

    let lending_market = read_pubkey(data, OBLIGATION_LENDING_MARKET_OFFSET);
    let owner = read_pubkey(data, OBLIGATION_OWNER_OFFSET);

    let mut deposits = Vec::new();
    for i in 0..OBLIGATION_DEPOSIT_SLOTS {
        let off = OBLIGATION_DEPOSITS_OFFSET + i * OBLIGATION_DEPOSIT_SLOT_SIZE;
        let reserve = read_pubkey(data, off);
        if reserve == Pubkey::default() {
            continue; // empty slot
        }
        let deposited_amount = u64::from_le_bytes(data[off + 32..off + 40].try_into().unwrap());
        let market_value_sf = u128::from_le_bytes(data[off + 40..off + 56].try_into().unwrap());
        deposits.push(ObligationDeposit {
            reserve,
            deposited_amount,
            market_value_sf,
        });
    }

    let mut borrows = Vec::new();
    for i in 0..OBLIGATION_BORROW_SLOTS {
        let off = OBLIGATION_BORROWS_OFFSET + i * OBLIGATION_BORROW_SLOT_SIZE;
        let reserve = read_pubkey(data, off);
        if reserve == Pubkey::default() {
            continue;
        }
        // After reserve(32) + cumulative_borrow_rate_bsf(16) + padding(8) = 56,
        // borrowed_amount_sf is at slot+56..72, market_value_sf at 72..88,
        // borrow_factor_adjusted_market_value_sf at 88..104.
        let borrowed_amount_sf = u128::from_le_bytes(data[off + 56..off + 72].try_into().unwrap());
        // Skip closed positions — klend keeps the reserve pubkey + stale per-slot
        // market_value after a borrow is fully repaid; only the aggregate fields
        // get zeroed. Filtering by borrowed_amount_sf == 0 keeps the per-borrow
        // list aligned with what's actually outstanding.
        if borrowed_amount_sf == 0 {
            continue;
        }
        let market_value_sf = u128::from_le_bytes(data[off + 72..off + 88].try_into().unwrap());
        let bfa_market_value_sf =
            u128::from_le_bytes(data[off + 88..off + 104].try_into().unwrap());
        borrows.push(ObligationBorrow {
            reserve,
            borrowed_amount_sf,
            market_value_sf,
            borrow_factor_adjusted_market_value_sf: bfa_market_value_sf,
        });
    }

    let deposited_value_sf = u128::from_le_bytes(
        data[OBLIGATION_DEPOSITED_VALUE_OFFSET..OBLIGATION_DEPOSITED_VALUE_OFFSET + 16]
            .try_into()
            .unwrap(),
    );

    let off = OBLIGATION_AGGREGATE_OFFSET;
    let borrow_factor_adjusted_debt_value_sf =
        u128::from_le_bytes(data[off..off + 16].try_into().unwrap());
    let borrowed_assets_market_value_sf =
        u128::from_le_bytes(data[off + 16..off + 32].try_into().unwrap());
    let allowed_borrow_value_sf = u128::from_le_bytes(data[off + 32..off + 48].try_into().unwrap());
    let unhealthy_borrow_value_sf =
        u128::from_le_bytes(data[off + 48..off + 64].try_into().unwrap());

    Ok(DecodedObligation {
        address,
        lending_market,
        owner,
        deposits,
        borrows,
        deposited_value_sf,
        borrow_factor_adjusted_debt_value_sf,
        borrowed_assets_market_value_sf,
        allowed_borrow_value_sf,
        unhealthy_borrow_value_sf,
    })
}

/// Fetch and decode an obligation account by pubkey. Returns `Ok(None)` if the
/// account doesn't exist on chain (user has not initialized an obligation).
pub async fn fetch_obligation(
    rpc: &RpcClient,
    obligation: &Pubkey,
) -> Result<Option<DecodedObligation>> {
    let accounts = rpc
        .get_multiple_accounts(&[*obligation])
        .await
        .with_context(|| format!("fetch obligation {obligation}"))?;
    let Some(account) = accounts.into_iter().next().flatten() else {
        return Ok(None);
    };
    Ok(Some(decode_obligation(*obligation, &account.data)?))
}

/// Query the user's obligation and compute current LTV in basis points.
///
/// LTV = `borrowed_assets_market_value_sf / deposited_value_sf`. Both values
/// are scaled fractions (sf, divide by 2^60 for real numbers); the scaling
/// cancels in the ratio. Result is clamped to `u16::MAX`.
///
/// Returns `Ok(0)` if:
///  * the obligation account does not yet exist on chain (fresh user, no
///    deposit ever made), or
///  * the obligation exists but has no deposits (`deposited_value_sf == 0`).
///
/// Both cases are valid pre-conditions for a leverage loop's first round.
pub async fn query_position_ltv_bps(
    rpc: &RpcClient,
    user: Pubkey,
    lending_market: Pubkey,
) -> Result<u16> {
    let obligation = crate::protocols::kamino::derive_user_obligation(&user, &lending_market);
    let decoded = match fetch_obligation(rpc, &obligation).await? {
        Some(d) => d,
        None => return Ok(0),
    };
    if decoded.deposited_value_sf == 0 {
        return Ok(0);
    }
    let ratio_bps = decoded
        .borrowed_assets_market_value_sf
        .saturating_mul(10_000)
        .checked_div(decoded.deposited_value_sf)
        .unwrap_or(0);
    Ok(ratio_bps.min(u16::MAX as u128) as u16)
}

/// Return `true` if the klend `UserMetadata` PDA for `user` exists on-chain.
/// A non-existent user_metadata means `initialize_obligation` will fail —
/// callers should prepend `init_user_metadata_ix` in that case.
pub async fn user_metadata_exists(rpc: &RpcClient, user: &Pubkey) -> bool {
    let pda = crate::protocols::kamino::derive_user_metadata(user);
    rpc.get_account(&pda).await.is_ok()
}

/// Return `true` if the obligation farm user state account for `(farm, obligation)`
/// already exists on-chain (owned by the Kamino Farms program).
/// When `false`, callers must prepend `init_obligation_farms_for_reserve_ix`.
pub async fn obligation_farm_state_exists(
    rpc: &RpcClient,
    farm: &Pubkey,
    user: &Pubkey,
    lending_market: &Pubkey,
) -> bool {
    let obligation = crate::protocols::kamino::derive_user_obligation(user, lending_market);
    let pda = crate::protocols::kamino::derive_obligation_farm_user_state(farm, &obligation);
    match rpc.get_account(&pda).await {
        Ok(acct) => acct.owner.to_string() != "11111111111111111111111111111111",
        Err(_) => false,
    }
}

/// Fetch the Kamino `Reserve` account at `reserve_pubkey` and decode the
/// sub-accounts needed to build deposit/withdraw instructions.
///
/// `expected_lending_market` is checked against the decoded lending_market
/// field as a sanity guard (catches wrong reserve pubkey at startup).
pub async fn load_reserve(
    rpc: &RpcClient,
    reserve_pubkey: &Pubkey,
    liquidity_mint: Pubkey,
    expected_lending_market: &Pubkey,
) -> Result<ReserveAccounts> {
    let data = rpc
        .get_account_data(reserve_pubkey)
        .await
        .with_context(|| format!("fetch reserve account {reserve_pubkey}"))?;

    if data.len() < MIN_RESERVE_SIZE {
        bail!(
            "reserve account {reserve_pubkey} is only {} bytes, expected >= {MIN_RESERVE_SIZE}",
            data.len()
        );
    }

    if data[0..8] != RESERVE_DISCRIMINATOR {
        bail!(
            "reserve account {reserve_pubkey} has wrong discriminator {:?}; not a klend Reserve",
            &data[0..8]
        );
    }

    let decoded_market = read_pubkey(&data, LENDING_MARKET_OFFSET);
    if &decoded_market != expected_lending_market {
        bail!(
            "reserve {reserve_pubkey} belongs to market {decoded_market}, expected {expected_lending_market}"
        );
    }

    let lending_market_authority =
        crate::protocols::kamino::derive_lending_market_authority(expected_lending_market);

    // Read scope oracle; fall back to Pubkey::default() if the data is shorter
    // than expected (devnet reserves may be smaller — simulations will reject).
    let scope_prices = if data.len() >= SCOPE_ORACLE_OFFSET + 32 {
        read_pubkey(&data, SCOPE_ORACLE_OFFSET)
    } else {
        Pubkey::default()
    };

    Ok(ReserveAccounts {
        reserve: *reserve_pubkey,
        lending_market: *expected_lending_market,
        lending_market_authority,
        liquidity_mint,
        liquidity_supply: read_pubkey(&data, LIQUIDITY_SUPPLY_VAULT_OFFSET),
        fee_receiver: read_pubkey(&data, LIQUIDITY_FEE_VAULT_OFFSET),
        collateral_mint: read_pubkey(&data, COLLATERAL_MINT_OFFSET),
        collateral_supply: read_pubkey(&data, COLLATERAL_SUPPLY_VAULT_OFFSET),
        scope_prices,
        farm_collateral: read_pubkey(&data, FARM_COLLATERAL_OFFSET),
    })
}

#[cfg(test)]
mod obligation_tests {
    use super::*;

    fn make_obligation_buf(
        market: &Pubkey,
        owner: &Pubkey,
        deposits: &[(Pubkey, u64, u128)],
        borrows: &[(Pubkey, u128, u128, u128)],
        deposited_value_sf: u128,
        bfa_debt_sf: u128,
        market_debt_sf: u128,
        allowed_sf: u128,
        unhealthy_sf: u128,
    ) -> Vec<u8> {
        let mut buf = vec![0u8; OBLIGATION_MIN_SIZE];
        buf[0..8].copy_from_slice(&OBLIGATION_DISCRIMINATOR);
        buf[OBLIGATION_LENDING_MARKET_OFFSET..OBLIGATION_LENDING_MARKET_OFFSET + 32]
            .copy_from_slice(&market.to_bytes());
        buf[OBLIGATION_OWNER_OFFSET..OBLIGATION_OWNER_OFFSET + 32]
            .copy_from_slice(&owner.to_bytes());

        for (i, (reserve, amt, mv)) in deposits.iter().enumerate() {
            let off = OBLIGATION_DEPOSITS_OFFSET + i * OBLIGATION_DEPOSIT_SLOT_SIZE;
            buf[off..off + 32].copy_from_slice(&reserve.to_bytes());
            buf[off + 32..off + 40].copy_from_slice(&amt.to_le_bytes());
            buf[off + 40..off + 56].copy_from_slice(&mv.to_le_bytes());
        }

        buf[OBLIGATION_DEPOSITED_VALUE_OFFSET..OBLIGATION_DEPOSITED_VALUE_OFFSET + 16]
            .copy_from_slice(&deposited_value_sf.to_le_bytes());

        for (i, (reserve, ba_sf, mv_sf, bfa_sf)) in borrows.iter().enumerate() {
            let off = OBLIGATION_BORROWS_OFFSET + i * OBLIGATION_BORROW_SLOT_SIZE;
            buf[off..off + 32].copy_from_slice(&reserve.to_bytes());
            buf[off + 56..off + 72].copy_from_slice(&ba_sf.to_le_bytes());
            buf[off + 72..off + 88].copy_from_slice(&mv_sf.to_le_bytes());
            buf[off + 88..off + 104].copy_from_slice(&bfa_sf.to_le_bytes());
        }

        buf[OBLIGATION_AGGREGATE_OFFSET..OBLIGATION_AGGREGATE_OFFSET + 16]
            .copy_from_slice(&bfa_debt_sf.to_le_bytes());
        buf[OBLIGATION_AGGREGATE_OFFSET + 16..OBLIGATION_AGGREGATE_OFFSET + 32]
            .copy_from_slice(&market_debt_sf.to_le_bytes());
        buf[OBLIGATION_AGGREGATE_OFFSET + 32..OBLIGATION_AGGREGATE_OFFSET + 48]
            .copy_from_slice(&allowed_sf.to_le_bytes());
        buf[OBLIGATION_AGGREGATE_OFFSET + 48..OBLIGATION_AGGREGATE_OFFSET + 64]
            .copy_from_slice(&unhealthy_sf.to_le_bytes());
        buf
    }

    #[test]
    fn decode_obligation_extracts_market_and_owner() {
        let market = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let buf = make_obligation_buf(&market, &owner, &[], &[], 0, 0, 0, 0, 0);
        let addr = Pubkey::new_unique();
        let o = decode_obligation(addr, &buf).expect("decode");
        assert_eq!(o.address, addr);
        assert_eq!(o.lending_market, market);
        assert_eq!(o.owner, owner);
        assert!(o.deposits.is_empty());
        assert!(o.borrows.is_empty());
    }

    #[test]
    fn decode_obligation_skips_empty_deposit_slots() {
        let r1 = Pubkey::new_unique();
        let r2 = Pubkey::new_unique();
        let mut buf = make_obligation_buf(
            &Pubkey::default(),
            &Pubkey::default(),
            &[(r1, 1_000_000_000, 1u128 << 60)],
            &[],
            0,
            0,
            0,
            0,
            0,
        );
        // Add slot 2 with another deposit (skipping slot 1)
        let off = OBLIGATION_DEPOSITS_OFFSET + 2 * OBLIGATION_DEPOSIT_SLOT_SIZE;
        buf[off..off + 32].copy_from_slice(&r2.to_bytes());
        buf[off + 32..off + 40].copy_from_slice(&500u64.to_le_bytes());
        buf[off + 40..off + 56].copy_from_slice(&(2u128 << 60).to_le_bytes());

        let o = decode_obligation(Pubkey::new_unique(), &buf).expect("decode");
        assert_eq!(o.deposits.len(), 2, "should skip empty slot 1");
        assert_eq!(o.deposits[0].reserve, r1);
        assert_eq!(o.deposits[0].deposited_amount, 1_000_000_000);
        assert_eq!(o.deposits[1].reserve, r2);
        assert_eq!(o.deposits[1].deposited_amount, 500);
    }

    #[test]
    fn decode_obligation_decodes_borrow_market_value_at_correct_offset() {
        let r = Pubkey::new_unique();
        let buf = make_obligation_buf(
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &[],
            &[(r, 1234u128 << 60, 5678u128 << 60, 9012u128 << 60)],
            0,
            0,
            0,
            0,
            0,
        );
        let o = decode_obligation(Pubkey::new_unique(), &buf).expect("decode");
        assert_eq!(o.borrows.len(), 1);
        assert_eq!(o.borrows[0].reserve, r);
        assert_eq!(o.borrows[0].borrowed_amount_sf, 1234u128 << 60);
        assert_eq!(o.borrows[0].market_value_sf, 5678u128 << 60);
        assert_eq!(
            o.borrows[0].borrow_factor_adjusted_market_value_sf,
            9012u128 << 60
        );
    }

    #[test]
    fn decode_obligation_skips_closed_borrow_slots() {
        let r_open = Pubkey::new_unique();
        let r_closed = Pubkey::new_unique();
        // First slot: closed (borrowed_amount_sf=0, but reserve+market_value still set).
        // Second slot: active.
        let buf = make_obligation_buf(
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &[],
            &[
                (r_closed, 0u128, 999u128 << 60, 999u128 << 60),
                (r_open, 1234u128 << 60, 5678u128 << 60, 9012u128 << 60),
            ],
            0,
            0,
            0,
            0,
            0,
        );
        let o = decode_obligation(Pubkey::new_unique(), &buf).expect("decode");
        assert_eq!(
            o.borrows.len(),
            1,
            "closed borrow with stale market_value should be filtered"
        );
        assert_eq!(o.borrows[0].reserve, r_open);
    }

    #[test]
    fn decode_obligation_decodes_aggregate_fields() {
        let buf = make_obligation_buf(
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &[],
            &[],
            100u128 << 60,
            10u128 << 60,
            12u128 << 60,
            80u128 << 60,
            90u128 << 60,
        );
        let o = decode_obligation(Pubkey::new_unique(), &buf).expect("decode");
        assert_eq!(o.deposited_value_sf, 100u128 << 60);
        assert_eq!(o.borrow_factor_adjusted_debt_value_sf, 10u128 << 60);
        assert_eq!(o.borrowed_assets_market_value_sf, 12u128 << 60);
        assert_eq!(o.allowed_borrow_value_sf, 80u128 << 60);
        assert_eq!(o.unhealthy_borrow_value_sf, 90u128 << 60);
    }

    #[test]
    fn decode_obligation_rejects_wrong_disc() {
        let mut buf = vec![0u8; OBLIGATION_MIN_SIZE];
        buf[0] = 0xff;
        assert!(decode_obligation(Pubkey::new_unique(), &buf).is_err());
    }

    #[test]
    fn decode_obligation_rejects_too_small() {
        let mut buf = vec![0u8; 100];
        buf[0..8].copy_from_slice(&OBLIGATION_DISCRIMINATOR);
        assert!(decode_obligation(Pubkey::new_unique(), &buf).is_err());
    }
}
