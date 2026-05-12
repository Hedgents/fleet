//! Adrena perps — open / close short position instruction builders.
//!
//! Adrena is the SOL short hedge venue in our portfolio strategy (Pillar 2).
//! There is a single `main-pool` with 4 active custodies indexed by position:
//! USDC(0), BONK(1), JitoSOL(2), WBTC(3). Shorting "SOL" really means shorting
//! JitoSOL (which tracks SOL closely).
//!
//! ## Account ordering (verified against on-chain Anchor IDL on 2026-05-04)
//!
//! `openPositionShort` (15 named accounts):
//! ```text
//! [ 0] owner                              (readonly)         the position's owner
//! [ 1] caller                             (signer)           who pays for the call (== owner for self-managed)
//! [ 2] payer                              (writable, signer) lamport rent payer
//! [ 3] funding_account                    (writable)         user's collateral mint ATA (USDC)
//! [ 4] transfer_authority                 (readonly)         PDA(["transfer_authority"])
//! [ 5] cortex                             (writable)         PDA(["cortex"])
//! [ 6] pool                               (writable)         the main pool
//! [ 7] position                           (writable)         PDA(["position", owner, pool, custody, [side]])
//! [ 8] custody                            (writable)         the asset to short (JitoSOL custody)
//! [ 9] oracle                             (writable)         PDA(["oracle"])
//! [10] collateral_custody                 (writable)         the collateral asset's custody (USDC custody)
//! [11] collateral_custody_token_account   (writable)         collateral custody's token vault
//! [12] system_program                     (readonly)
//! [13] token_program                      (readonly)
//! [14] adrena_program                     (readonly)         the Adrena program id (self)
//! ```
//!
//! `closePositionShort` (15 named accounts) — same ordering with two swaps:
//! `funding_account` becomes `receiving_account`, and `payer/caller` collapse
//! into one (caller signs and is writable). Trailing two optional accounts
//! (`user_profile`, `referrer_profile`) are passed as `None` (program ID
//! placeholder) when not used.
//!
//! ## Position PDA seeds (verified by reverse-engineering an active position)
//!
//! ```text
//! seeds = ["position", owner, pool, custody, [side_byte]]
//! side_byte = 1 (Long) or 2 (Short)
//! ```

use borsh::BorshSerialize;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;

use crate::{
    constants::{ADRENA_PROGRAM_ID, SYSTEM_PROGRAM_ID, TOKEN_PROGRAM_ID},
    util::{anchor_discriminator, ata},
    Error, Result,
};

// ── Types ───────────────────────────────────────────────────────────────────

/// Position side. The byte value is what gets serialized into the position
/// PDA seed; not exposed in any wire arg directly (the side is encoded in
/// which open instruction you call).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Long = 1,
    Short = 2,
}

impl Side {
    fn as_seed(&self) -> [u8; 1] {
        [*self as u8]
    }
}

/// Subset of Adrena's `Custody` account fields the daemon needs to build
/// open/close instructions. All four pubkeys are mandatory.
#[derive(Debug, Clone)]
pub struct CustodyMeta {
    /// The Custody account address itself.
    pub address: Pubkey,
    /// SPL mint of the underlying asset.
    pub mint: Pubkey,
    /// Custody's token vault.
    pub token_account: Pubkey,
    /// Decimals of the underlying mint.
    pub decimals: u8,
    /// Whether the custody is treated as a stablecoin in pool math.
    pub is_stable: bool,
}

/// Aggregate view of Adrena's main pool — pool address, the static set of
/// custodies needed for shorts (trade asset + collateral), and the program
/// PDAs that every position-management ix needs.
#[derive(Debug, Clone)]
pub struct PoolMeta {
    pub pool: Pubkey,
    pub cortex: Pubkey,
    pub transfer_authority: Pubkey,
    pub oracle: Pubkey,
    /// JitoSOL custody — the SOL-direction asset.
    pub jitosol_custody: CustodyMeta,
    /// USDC custody — the collateral asset for SOL hedge shorts.
    pub usdc_custody: CustodyMeta,
}

// ── PDA derivation ──────────────────────────────────────────────────────────

pub fn derive_cortex() -> Pubkey {
    Pubkey::find_program_address(&[b"cortex"], &ADRENA_PROGRAM_ID).0
}

pub fn derive_transfer_authority() -> Pubkey {
    Pubkey::find_program_address(&[b"transfer_authority"], &ADRENA_PROGRAM_ID).0
}

pub fn derive_oracle() -> Pubkey {
    Pubkey::find_program_address(&[b"oracle"], &ADRENA_PROGRAM_ID).0
}

/// Position PDA: `["position", owner, pool, custody, [side_byte]]`.
pub fn derive_position(owner: &Pubkey, pool: &Pubkey, custody: &Pubkey, side: Side) -> Pubkey {
    let side_seed = side.as_seed();
    Pubkey::find_program_address(
        &[
            b"position",
            owner.as_ref(),
            pool.as_ref(),
            custody.as_ref(),
            &side_seed,
        ],
        &ADRENA_PROGRAM_ID,
    )
    .0
}

// ── Instruction args ────────────────────────────────────────────────────────

#[derive(BorshSerialize)]
struct OpenPositionShortParams {
    /// Limit price (Adrena uses 6-decimal USD scaling internally for prices).
    /// Set to a generous bound; the program rejects if entry exceeds.
    price: u64,
    /// Collateral amount in raw units of the collateral mint (USDC = 6 dec).
    collateral: u64,
    /// Leverage in basis points (e.g. 20_000 = 2.0x).
    leverage: u32,
    /// Off-chain Chaos Labs price proof. We always pass `None` and rely on
    /// the on-chain oracle account (which Adrena keepers refresh).
    oracle_prices: Option<()>,
}

#[derive(BorshSerialize)]
struct ClosePositionShortParams {
    /// Optional limit price; `None` = market-close at oracle price.
    price: Option<u64>,
    /// Off-chain price proof — `None` for on-chain oracle path.
    oracle_prices: Option<()>,
    /// Percentage to close in basis points (10_000 = 100%).
    percentage: u64,
}

#[derive(BorshSerialize)]
struct AddCollateralShortParams {
    /// Raw USDC units (6 decimals) to add to the position's collateral.
    collateral: u64,
    /// Off-chain price proof — `None` for on-chain oracle path.
    oracle_prices: Option<()>,
}

#[derive(BorshSerialize)]
struct RemoveCollateralShortParams {
    /// Collateral USD value to remove, in 6-decimal USD scaling
    /// (e.g. 5_000_000 = $5.00). Adrena translates back to USDC tokens
    /// internally based on the current oracle price.
    collateral_usd: u64,
    /// Off-chain price proof — `None` for on-chain oracle path.
    oracle_prices: Option<()>,
}

// ── open_position_short ─────────────────────────────────────────────────────

/// Build the instruction sequence to open a short position on `pool.jitosol_custody`
/// using `pool.usdc_custody` as collateral.
///
/// `collateral_amount` is in raw USDC units (6 decimals). `leverage_bps` is in
/// basis points: 10_000 = 1x, 20_000 = 2x, etc.
///
/// `max_entry_price_usd_e6` is the user's price-slippage upper bound in
/// 6-decimal USD scaling (e.g. SOL=$80 → 80_000_000). Pass a high value
/// (e.g. `u64::MAX`) to accept any entry price; production callers should
/// query `getEntryPriceAndFee` first.
///
/// Returns:
/// 1. Idempotent ATA-create for the user's USDC ATA (no-op if exists)
/// 2. The `openPositionShort` instruction
pub fn open_position_short_ix(
    user: &Pubkey,
    pool: &PoolMeta,
    collateral_amount: u64,
    leverage_bps: u32,
    max_entry_price_usd_e6: u64,
) -> Result<Vec<Instruction>> {
    if collateral_amount == 0 {
        return Err(Error::ZeroAmount);
    }
    if leverage_bps == 0 {
        return Err(Error::ZeroAmount);
    }

    let user_collateral_ata = ata(user, &pool.usdc_custody.mint);
    let position = derive_position(user, &pool.pool, &pool.jitosol_custody.address, Side::Short);

    let mut ixs = Vec::with_capacity(2);

    ixs.push(create_associated_token_account_idempotent(
        user,
        user,
        &pool.usdc_custody.mint,
        &TOKEN_PROGRAM_ID,
    ));

    let mut data = anchor_discriminator("global", "open_position_short").to_vec();
    OpenPositionShortParams {
        price: max_entry_price_usd_e6,
        collateral: collateral_amount,
        leverage: leverage_bps,
        oracle_prices: None,
    }
    .serialize(&mut data)
    .map_err(|_| Error::Overflow)?;

    let accounts = vec![
        AccountMeta::new_readonly(*user, false),      // [0] owner
        AccountMeta::new_readonly(*user, true), // [1] caller (signer, == owner for self-managed)
        AccountMeta::new(*user, true),          // [2] payer (writable signer)
        AccountMeta::new(user_collateral_ata, false), // [3] funding_account
        AccountMeta::new_readonly(pool.transfer_authority, false), // [4] transfer_authority
        AccountMeta::new(pool.cortex, false),   // [5] cortex (w)
        AccountMeta::new(pool.pool, false),     // [6] pool (w)
        AccountMeta::new(position, false),      // [7] position (w, PDA)
        AccountMeta::new(pool.jitosol_custody.address, false), // [8] custody (w)
        AccountMeta::new(pool.oracle, false),   // [9] oracle (w, PDA)
        AccountMeta::new(pool.usdc_custody.address, false), // [10] collateral_custody (w)
        AccountMeta::new(pool.usdc_custody.token_account, false), // [11] collateral_custody_token_account (w)
        AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),      // [12] system_program
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),       // [13] token_program
        AccountMeta::new_readonly(ADRENA_PROGRAM_ID, false),      // [14] adrena_program (self)
    ];

    ixs.push(Instruction {
        program_id: ADRENA_PROGRAM_ID,
        accounts,
        data,
    });

    Ok(ixs)
}

// ── close_position_short ────────────────────────────────────────────────────

/// Build the instruction sequence to close (or partially close) a short
/// position on `pool.jitosol_custody`.
///
/// `percentage_bps`: 10_000 = full close, 5_000 = half close, etc.
/// `min_exit_price_usd_e6`: pass `None` for market close.
///
/// Returns just the `closePositionShort` instruction — the user's USDC ATA
/// must already exist (it was created when the position opened).
pub fn close_position_short_ix(
    user: &Pubkey,
    pool: &PoolMeta,
    percentage_bps: u64,
    min_exit_price_usd_e6: Option<u64>,
) -> Result<Vec<Instruction>> {
    if percentage_bps == 0 || percentage_bps > 10_000 {
        return Err(Error::ZeroAmount);
    }

    let user_receiving_ata = ata(user, &pool.usdc_custody.mint);
    let position = derive_position(user, &pool.pool, &pool.jitosol_custody.address, Side::Short);

    let mut data = anchor_discriminator("global", "close_position_short").to_vec();
    ClosePositionShortParams {
        price: min_exit_price_usd_e6,
        oracle_prices: None,
        percentage: percentage_bps,
    }
    .serialize(&mut data)
    .map_err(|_| Error::Overflow)?;

    let accounts = vec![
        AccountMeta::new(*user, true),  // [0] caller (writable signer)
        AccountMeta::new(*user, false), // [1] owner (writable, lamport refunds)
        AccountMeta::new(user_receiving_ata, false), // [2] receiving_account (w)
        AccountMeta::new_readonly(pool.transfer_authority, false), // [3] transfer_authority
        AccountMeta::new(pool.cortex, false), // [4] cortex (w)
        AccountMeta::new(pool.pool, false), // [5] pool (w)
        AccountMeta::new(position, false), // [6] position (w)
        AccountMeta::new(pool.jitosol_custody.address, false), // [7] custody (w)
        AccountMeta::new(pool.oracle, false), // [8] oracle (w)
        AccountMeta::new(pool.usdc_custody.address, false), // [9] collateral_custody (w)
        AccountMeta::new(pool.usdc_custody.token_account, false), // [10] collateral_custody_token_account (w)
        // Optional accounts: pass program-id as None placeholder (Anchor convention)
        AccountMeta::new(ADRENA_PROGRAM_ID, false), // [11] user_profile (opt, w)
        AccountMeta::new(ADRENA_PROGRAM_ID, false), // [12] referrer_profile (opt, w)
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false), // [13] token_program
        AccountMeta::new_readonly(ADRENA_PROGRAM_ID, false), // [14] adrena_program
    ];

    Ok(vec![Instruction {
        program_id: ADRENA_PROGRAM_ID,
        accounts,
        data,
    }])
}

// ── add_collateral_short ────────────────────────────────────────────────────

/// Add `collateral_usdc_amount` raw USDC units to an existing JitoSOL short
/// position's collateral cushion. Reduces the position's effective leverage
/// without changing position size.
///
/// Returns the single instruction. The position must already exist.
pub fn add_collateral_short_ix(
    user: &Pubkey,
    pool: &PoolMeta,
    collateral_usdc_amount: u64,
) -> Result<Instruction> {
    if collateral_usdc_amount == 0 {
        return Err(Error::ZeroAmount);
    }

    let user_funding_ata = ata(user, &pool.usdc_custody.mint);
    let position = derive_position(user, &pool.pool, &pool.jitosol_custody.address, Side::Short);

    let mut data = anchor_discriminator("global", "add_collateral_short").to_vec();
    AddCollateralShortParams {
        collateral: collateral_usdc_amount,
        oracle_prices: None,
    }
    .serialize(&mut data)
    .map_err(|_| Error::Overflow)?;

    let accounts = vec![
        AccountMeta::new(*user, true), // [0] owner (writable signer)
        AccountMeta::new(user_funding_ata, false), // [1] funding_account
        AccountMeta::new_readonly(pool.transfer_authority, false), // [2] transfer_authority
        AccountMeta::new(pool.cortex, false), // [3] cortex
        AccountMeta::new(pool.pool, false), // [4] pool
        AccountMeta::new(position, false), // [5] position
        AccountMeta::new(pool.jitosol_custody.address, false), // [6] custody (JitoSOL — the short asset)
        AccountMeta::new(pool.oracle, false),                  // [7] oracle
        AccountMeta::new(pool.usdc_custody.address, false),    // [8] collateral_custody (USDC)
        AccountMeta::new(pool.usdc_custody.token_account, false), // [9] collateral_custody_token_account
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),       // [10] token_program
        AccountMeta::new_readonly(ADRENA_PROGRAM_ID, false),      // [11] adrena_program
    ];

    Ok(Instruction {
        program_id: ADRENA_PROGRAM_ID,
        accounts,
        data,
    })
}

// ── remove_collateral_short ─────────────────────────────────────────────────

/// Remove `collateral_usd_e6` USD-denominated collateral (6-decimal scaling)
/// from an existing JitoSOL short position. Increases the position's
/// effective leverage. The program rejects if it would breach the position's
/// max leverage.
///
/// Note: arg is in **USD** (6-decimal) not raw USDC tokens — Adrena does the
/// translation internally based on current oracle price.
pub fn remove_collateral_short_ix(
    user: &Pubkey,
    pool: &PoolMeta,
    collateral_usd_e6: u64,
) -> Result<Instruction> {
    if collateral_usd_e6 == 0 {
        return Err(Error::ZeroAmount);
    }

    let user_receiving_ata = ata(user, &pool.usdc_custody.mint);
    let position = derive_position(user, &pool.pool, &pool.jitosol_custody.address, Side::Short);

    let mut data = anchor_discriminator("global", "remove_collateral_short").to_vec();
    RemoveCollateralShortParams {
        collateral_usd: collateral_usd_e6,
        oracle_prices: None,
    }
    .serialize(&mut data)
    .map_err(|_| Error::Overflow)?;

    // Note: `removeCollateralShort` swaps the order of [10] and [11] vs
    // `addCollateralShort` — adrenaProgram comes BEFORE tokenProgram here.
    // Verified against IDL.
    let accounts = vec![
        AccountMeta::new(*user, true), // [0] owner (writable signer)
        AccountMeta::new(user_receiving_ata, false), // [1] receiving_account
        AccountMeta::new_readonly(pool.transfer_authority, false), // [2] transfer_authority
        AccountMeta::new(pool.cortex, false), // [3] cortex
        AccountMeta::new(pool.pool, false), // [4] pool
        AccountMeta::new(position, false), // [5] position
        AccountMeta::new(pool.jitosol_custody.address, false), // [6] custody (JitoSOL)
        AccountMeta::new(pool.oracle, false), // [7] oracle
        AccountMeta::new(pool.usdc_custody.address, false), // [8] collateral_custody (USDC)
        AccountMeta::new(pool.usdc_custody.token_account, false), // [9] collateral_custody_token_account
        AccountMeta::new_readonly(ADRENA_PROGRAM_ID, false), // [10] adrena_program (BEFORE token_program!)
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),  // [11] token_program
    ];

    Ok(Instruction {
        program_id: ADRENA_PROGRAM_ID,
        accounts,
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::{
        ADRENA_CUSTODY_JITOSOL, ADRENA_CUSTODY_USDC, ADRENA_MAIN_POOL, USDC_MINT,
    };
    use std::str::FromStr;

    fn dummy_custody(addr: Pubkey, mint: Pubkey, decimals: u8, stable: bool) -> CustodyMeta {
        CustodyMeta {
            address: addr,
            mint,
            token_account: Pubkey::new_unique(),
            decimals,
            is_stable: stable,
        }
    }

    fn dummy_pool() -> PoolMeta {
        PoolMeta {
            pool: ADRENA_MAIN_POOL,
            cortex: derive_cortex(),
            transfer_authority: derive_transfer_authority(),
            oracle: derive_oracle(),
            jitosol_custody: dummy_custody(ADRENA_CUSTODY_JITOSOL, Pubkey::new_unique(), 9, false),
            usdc_custody: dummy_custody(ADRENA_CUSTODY_USDC, USDC_MINT, 6, true),
        }
    }

    #[test]
    fn cortex_pda_matches_known_mainnet_address() {
        let expected = Pubkey::from_str("Dhz8Ta79hgyUbaRcu7qHMnqMfY47kQHfHt2s42D9dC4e").unwrap();
        assert_eq!(derive_cortex(), expected);
    }

    #[test]
    fn transfer_authority_pda_matches_known_mainnet() {
        let expected = Pubkey::from_str("4o3qAErcapJ6gRLh1m1x4saoLLieWDu7Rx3wpwLc7Zk9").unwrap();
        assert_eq!(derive_transfer_authority(), expected);
    }

    #[test]
    fn oracle_pda_matches_known_mainnet() {
        let expected = Pubkey::from_str("GEm9TZP7BL8rTz1JDy6X74PL595zr1putA9BXC8ehDmU").unwrap();
        assert_eq!(derive_oracle(), expected);
    }

    #[test]
    fn position_pda_matches_real_short_position() {
        // Verified by reverse-engineering position
        // BAnKLuHW83hLPeL1CKqMwTfBzvjRWJPWjZDefDKrmyAS on 2026-05-04.
        let owner = Pubkey::from_str("EicVFscMkR1FxyNT1kEJbvgoXrmhZRzJtPMEGJacS62k").unwrap();
        let pool = Pubkey::from_str("4bQRutgDJs6vuh6ZcWaPVXiQaBzbHketjbCDjL4oRN34").unwrap();
        let custody = Pubkey::from_str("GZ9XfWwgTRhkma2Y91Q9r1XKotNXYjBnKKabj19rhT71").unwrap();
        let expected = Pubkey::from_str("BAnKLuHW83hLPeL1CKqMwTfBzvjRWJPWjZDefDKrmyAS").unwrap();
        assert_eq!(
            derive_position(&owner, &pool, &custody, Side::Short),
            expected
        );
    }

    #[test]
    fn open_short_rejects_zero_collateral() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        assert!(matches!(
            open_position_short_ix(&user, &pool, 0, 20_000, u64::MAX),
            Err(Error::ZeroAmount)
        ));
    }

    #[test]
    fn open_short_rejects_zero_leverage() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        assert!(matches!(
            open_position_short_ix(&user, &pool, 1_000_000, 0, u64::MAX),
            Err(Error::ZeroAmount)
        ));
    }

    #[test]
    fn open_short_returns_two_instructions() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let ixs = open_position_short_ix(&user, &pool, 1_000_000, 20_000, u64::MAX).expect("build");
        assert_eq!(ixs.len(), 2, "ATA-create + open");
        assert_eq!(ixs[1].program_id, ADRENA_PROGRAM_ID);
    }

    #[test]
    fn open_short_has_15_accounts_in_correct_order() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let ixs = open_position_short_ix(&user, &pool, 1_000_000, 20_000, u64::MAX).expect("build");
        let ix = ixs.last().unwrap();
        assert_eq!(ix.accounts.len(), 15);
        // Owner is account 0 — readonly, not signer (caller signs).
        assert_eq!(ix.accounts[0].pubkey, user);
        assert!(!ix.accounts[0].is_writable);
        // Caller is signer.
        assert!(ix.accounts[1].is_signer);
        // Payer is writable signer.
        assert!(ix.accounts[2].is_writable && ix.accounts[2].is_signer);
        // Critical PDAs in correct slots.
        assert_eq!(ix.accounts[4].pubkey, pool.transfer_authority);
        assert_eq!(ix.accounts[5].pubkey, pool.cortex);
        assert_eq!(ix.accounts[6].pubkey, pool.pool);
        assert_eq!(ix.accounts[8].pubkey, pool.jitosol_custody.address);
        assert_eq!(ix.accounts[9].pubkey, pool.oracle);
        assert_eq!(ix.accounts[10].pubkey, pool.usdc_custody.address);
        assert_eq!(ix.accounts[14].pubkey, ADRENA_PROGRAM_ID);
    }

    #[test]
    fn open_short_data_starts_with_anchor_discriminator() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let ixs =
            open_position_short_ix(&user, &pool, 1_000_000, 20_000, 100_000_000).expect("build");
        let ix = ixs.last().unwrap();
        // 8 disc + 8 price + 8 collateral + 4 leverage + 1 option-tag = 29 bytes
        assert_eq!(ix.data.len(), 29);
        assert_eq!(
            &ix.data[..8],
            &anchor_discriminator("global", "open_position_short")
        );
        // Option tag is None (0)
        assert_eq!(ix.data[28], 0);
    }

    #[test]
    fn close_short_rejects_zero_or_oversized_percentage() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        assert!(matches!(
            close_position_short_ix(&user, &pool, 0, None),
            Err(Error::ZeroAmount)
        ));
        assert!(matches!(
            close_position_short_ix(&user, &pool, 10_001, None),
            Err(Error::ZeroAmount)
        ));
    }

    #[test]
    fn close_short_returns_one_instruction_with_15_accounts() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let ixs = close_position_short_ix(&user, &pool, 10_000, None).expect("build");
        assert_eq!(ixs.len(), 1);
        assert_eq!(ixs[0].accounts.len(), 15);
        assert_eq!(ixs[0].program_id, ADRENA_PROGRAM_ID);
    }

    #[test]
    fn close_short_data_includes_optional_price() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        // With None price: 8 disc + 1 (Option<u64>=None) + 1 (Option<()>=None) + 8 percentage = 18
        let ix_none = &close_position_short_ix(&user, &pool, 10_000, None).unwrap()[0];
        assert_eq!(ix_none.data.len(), 18, "None price encodes as 1 tag byte");
        // With Some price: 8 disc + 1 tag + 8 price + 1 (oracle None) + 8 percentage = 26
        let ix_some = &close_position_short_ix(&user, &pool, 10_000, Some(80_000_000)).unwrap()[0];
        assert_eq!(ix_some.data.len(), 26, "Some price adds 8 bytes");
    }

    #[test]
    fn side_seed_byte_is_2_for_short() {
        assert_eq!(Side::Short.as_seed(), [2u8]);
        assert_eq!(Side::Long.as_seed(), [1u8]);
    }

    #[test]
    fn add_collateral_short_rejects_zero() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        assert!(matches!(
            add_collateral_short_ix(&user, &pool, 0),
            Err(Error::ZeroAmount)
        ));
    }

    #[test]
    fn add_collateral_short_has_12_accounts_in_correct_order() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let ix = add_collateral_short_ix(&user, &pool, 1_000_000).expect("build");
        assert_eq!(ix.accounts.len(), 12);
        // owner is writable signer
        assert!(ix.accounts[0].is_signer && ix.accounts[0].is_writable);
        assert_eq!(ix.accounts[2].pubkey, pool.transfer_authority);
        assert_eq!(ix.accounts[3].pubkey, pool.cortex);
        assert_eq!(ix.accounts[4].pubkey, pool.pool);
        assert_eq!(ix.accounts[6].pubkey, pool.jitosol_custody.address);
        assert_eq!(ix.accounts[7].pubkey, pool.oracle);
        assert_eq!(ix.accounts[8].pubkey, pool.usdc_custody.address);
        assert_eq!(ix.accounts[10].pubkey, TOKEN_PROGRAM_ID);
        assert_eq!(ix.accounts[11].pubkey, ADRENA_PROGRAM_ID);
    }

    #[test]
    fn add_collateral_short_data_starts_with_anchor_discriminator() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let ix = add_collateral_short_ix(&user, &pool, 5_000_000).expect("build");
        // 8 disc + 8 collateral + 1 oracle option-tag = 17 bytes
        assert_eq!(ix.data.len(), 17);
        assert_eq!(
            &ix.data[..8],
            &anchor_discriminator("global", "add_collateral_short")
        );
        assert_eq!(ix.data[16], 0, "Option<()> = None encodes as 0x00");
    }

    #[test]
    fn remove_collateral_short_rejects_zero() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        assert!(matches!(
            remove_collateral_short_ix(&user, &pool, 0),
            Err(Error::ZeroAmount)
        ));
    }

    #[test]
    fn remove_collateral_short_has_12_accounts_with_swapped_program_order() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let ix = remove_collateral_short_ix(&user, &pool, 1_000_000).expect("build");
        assert_eq!(ix.accounts.len(), 12);
        // The crucial detail: removeCollateralShort puts adrena_program at [10],
        // token_program at [11] — opposite of addCollateralShort.
        assert_eq!(ix.accounts[10].pubkey, ADRENA_PROGRAM_ID);
        assert_eq!(ix.accounts[11].pubkey, TOKEN_PROGRAM_ID);
    }

    #[test]
    fn remove_collateral_short_data_starts_with_anchor_discriminator() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let ix = remove_collateral_short_ix(&user, &pool, 5_000_000).expect("build");
        assert_eq!(ix.data.len(), 17);
        assert_eq!(
            &ix.data[..8],
            &anchor_discriminator("global", "remove_collateral_short")
        );
    }
}
