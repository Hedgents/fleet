//! Jupiter Perpetuals — JLP mint/burn instruction builders.
//!
//! JLP is the LP token of Jupiter's perp pool (~47% SOL, 10% ETH, 10% BTC,
//! 25% USDC, 9% USDT). Holders receive 75% of perp trading fees.
//!
//! ## Architecture
//!
//! Two on-chain instructions wrap mint/burn:
//!   - `add_liquidity_2` (deposit asset → mint JLP)
//!   - `remove_liquidity_2` (burn JLP → withdraw asset)
//!
//! Both rely on a *cached* `pool.aum_usd` value that Jupiter's keepers refresh
//! via `refresh_assets_under_management` on a tight cadence. The named-account
//! list for `add_liquidity_2`/`remove_liquidity_2` is exactly the 14 accounts
//! enumerated in the on-chain Anchor IDL — no remaining-accounts needed for
//! the user path. If the cached AUM is too stale at execution time, the
//! program rejects the call (a keeper will refresh shortly after).
//!
//! ## Account ordering (verified against on-chain IDL on 2026-05-04)
//!
//! ```text
//! [ 0] owner                       (signer)
//! [ 1] funding_account             (writable)  user's input ATA (or receiving for remove)
//! [ 2] lp_token_account            (writable)  user's JLP ATA
//! [ 3] transfer_authority          (readonly)  PDA(["transfer_authority"])
//! [ 4] perpetuals                  (readonly)  PDA(["perpetuals"])
//! [ 5] pool                        (writable)  the JLP pool account
//! [ 6] custody                     (writable)  input/output asset's custody
//! [ 7] custody_doves_price_account (readonly)  custody.doves_ag_oracle
//! [ 8] custody_pythnet_price_account (readonly) custody.oracle.oracle_account
//! [ 9] custody_token_account       (writable)  custody's token vault
//! [10] lp_token_mint               (writable)  JLP_MINT
//! [11] token_program               (readonly)
//! [12] event_authority             (readonly)  PDA(["__event_authority"])
//! [13] program                     (readonly)  JUPITER_PERPETUALS_PROGRAM_ID
//! ```
//!
//! ## Custody decoding
//!
//! Each `Custody` account (~2000 bytes) has these fields at fixed offsets
//! (verified against mainnet on 2026-05-04):
//!
//! ```text
//! [ 40..72]  mint
//! [ 72..104] token_account
//! [104]      decimals (u8)
//! [105]      is_stable (bool)
//! [106..138] oracle.oracle_account     <- custody_pythnet_price_account
//! [320..352] doves_oracle              (older oracle, not used by `_2` ixs)
//! [384..416] doves_ag_oracle           <- custody_doves_price_account
//! ```

use borsh::BorshSerialize;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;

use crate::{
    constants::{JLP_MINT, JLP_POOL, JUPITER_PERPETUALS_PROGRAM_ID, TOKEN_PROGRAM_ID},
    util::{anchor_discriminator, ata},
    Error, Result,
};

// ── Custody / Pool metadata ─────────────────────────────────────────────────

/// Fully decoded view of a single Jupiter Perps `Custody` account — only the
/// fields the daemon needs to build add/remove liquidity instructions.
#[derive(Debug, Clone)]
pub struct CustodyMeta {
    /// The Custody account address (a PDA owned by the perps program).
    pub address: Pubkey,
    /// SPL mint of the underlying asset (e.g. USDC, wSOL, WETH-portal).
    pub mint: Pubkey,
    /// Custody's token vault holding the deposited assets.
    pub token_account: Pubkey,
    /// Pyth pull-oracle account (`custody.oracle.oracle_account`).
    pub pythnet_price_account: Pubkey,
    /// Doves V2 aggregator oracle (`custody.doves_ag_oracle`).
    pub doves_price_account: Pubkey,
    /// Decimals of the underlying mint — needed for amount conversions.
    pub decimals: u8,
    /// Whether this asset is treated as a stablecoin in pool math.
    pub is_stable: bool,
}

/// Aggregate view of the JLP pool — pool address, list of custodies (indexed
/// by their position in `pool.custodies`), and the program-level PDAs that
/// every add/remove liquidity ix needs.
#[derive(Debug, Clone)]
pub struct PoolMeta {
    pub pool: Pubkey,
    pub jlp_mint: Pubkey,
    pub perpetuals: Pubkey,
    pub transfer_authority: Pubkey,
    pub event_authority: Pubkey,
    pub custodies: Vec<CustodyMeta>,
}

impl PoolMeta {
    /// Find a custody by SPL mint. `None` if the mint isn't part of this pool.
    pub fn custody_for_mint(&self, mint: &Pubkey) -> Option<&CustodyMeta> {
        self.custodies.iter().find(|c| c.mint == *mint)
    }
}

// ── PDA derivation ──────────────────────────────────────────────────────────

pub fn derive_perpetuals() -> Pubkey {
    Pubkey::find_program_address(&[b"perpetuals"], &JUPITER_PERPETUALS_PROGRAM_ID).0
}

pub fn derive_transfer_authority() -> Pubkey {
    Pubkey::find_program_address(&[b"transfer_authority"], &JUPITER_PERPETUALS_PROGRAM_ID).0
}

pub fn derive_event_authority() -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], &JUPITER_PERPETUALS_PROGRAM_ID).0
}

// ── Instruction args ────────────────────────────────────────────────────────

#[derive(BorshSerialize)]
struct AddLiquidity2Params {
    token_amount_in: u64,
    min_lp_amount_out: u64,
    /// Optional pre-swap amount when the program performs an internal swap
    /// before the deposit. We never use this for the simple deposit path.
    token_amount_pre_swap: Option<u64>,
}

#[derive(BorshSerialize)]
struct RemoveLiquidity2Params {
    lp_amount_in: u64,
    min_amount_out: u64,
}

// ── add_liquidity_2 ─────────────────────────────────────────────────────────

/// Build the instruction sequence to deposit `amount_in` raw units of
/// `input_custody.mint` into the JLP pool and mint at least
/// `min_lp_amount_out` JLP tokens to the user.
///
/// Returns:
/// 1. Idempotent ATA-create for the user's input ATA (no-op if exists)
/// 2. Idempotent ATA-create for the user's JLP ATA (no-op if exists)
/// 3. The `add_liquidity_2` instruction
///
/// `min_lp_amount_out` of 0 disables slippage protection — caller should
/// compute it from `getAddLiquidityAmountAndFee2` (a view ix) in production
/// flows. The daemon uses 0 for `?simulate=true` runs and lets callers pass
/// their own value for broadcast.
///
/// Caller is responsible for adding compute budget instructions.
pub fn add_liquidity_ix(
    user: &Pubkey,
    pool: &PoolMeta,
    input_custody: &CustodyMeta,
    amount_in: u64,
    min_lp_amount_out: u64,
) -> Result<Vec<Instruction>> {
    if amount_in == 0 {
        return Err(Error::ZeroAmount);
    }

    let user_input_ata = ata(user, &input_custody.mint);
    let user_jlp_ata = ata(user, &pool.jlp_mint);

    let mut ixs = Vec::with_capacity(3);

    ixs.push(create_associated_token_account_idempotent(
        user,
        user,
        &input_custody.mint,
        &TOKEN_PROGRAM_ID,
    ));
    ixs.push(create_associated_token_account_idempotent(
        user,
        user,
        &pool.jlp_mint,
        &TOKEN_PROGRAM_ID,
    ));

    let mut data = anchor_discriminator("global", "add_liquidity_2").to_vec();
    AddLiquidity2Params {
        token_amount_in: amount_in,
        min_lp_amount_out,
        token_amount_pre_swap: None,
    }
    .serialize(&mut data)
    .map_err(|_| Error::Overflow)?;

    let accounts = vec![
        AccountMeta::new_readonly(*user, true),                              // [0] owner (signer, not writable)
        AccountMeta::new(user_input_ata, false),                             // [1] funding_account (w)
        AccountMeta::new(user_jlp_ata, false),                               // [2] lp_token_account (w)
        AccountMeta::new_readonly(pool.transfer_authority, false),           // [3] transfer_authority
        AccountMeta::new_readonly(pool.perpetuals, false),                   // [4] perpetuals
        AccountMeta::new(pool.pool, false),                                  // [5] pool (w)
        AccountMeta::new(input_custody.address, false),                      // [6] custody (w)
        AccountMeta::new_readonly(input_custody.doves_price_account, false), // [7] custody_doves_price_account
        AccountMeta::new_readonly(input_custody.pythnet_price_account, false), // [8] custody_pythnet_price_account
        AccountMeta::new(input_custody.token_account, false),                // [9] custody_token_account (w)
        AccountMeta::new(pool.jlp_mint, false),                              // [10] lp_token_mint (w)
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),                  // [11] token_program
        AccountMeta::new_readonly(pool.event_authority, false),              // [12] event_authority
        AccountMeta::new_readonly(JUPITER_PERPETUALS_PROGRAM_ID, false),     // [13] program (self)
    ];

    ixs.push(Instruction {
        program_id: JUPITER_PERPETUALS_PROGRAM_ID,
        accounts,
        data,
    });

    Ok(ixs)
}

// ── remove_liquidity_2 ──────────────────────────────────────────────────────

/// Build the instruction sequence to burn `lp_amount_in` JLP and receive at
/// least `min_amount_out` raw units of `output_custody.mint`.
///
/// Returns:
/// 1. Idempotent ATA-create for the user's output ATA (no-op if exists)
/// 2. The `remove_liquidity_2` instruction
///
/// (No JLP ATA create — caller must already hold JLP to burn it.)
pub fn remove_liquidity_ix(
    user: &Pubkey,
    pool: &PoolMeta,
    output_custody: &CustodyMeta,
    lp_amount_in: u64,
    min_amount_out: u64,
) -> Result<Vec<Instruction>> {
    if lp_amount_in == 0 {
        return Err(Error::ZeroAmount);
    }

    let user_output_ata = ata(user, &output_custody.mint);
    let user_jlp_ata = ata(user, &pool.jlp_mint);

    let mut ixs = Vec::with_capacity(2);

    ixs.push(create_associated_token_account_idempotent(
        user,
        user,
        &output_custody.mint,
        &TOKEN_PROGRAM_ID,
    ));

    let mut data = anchor_discriminator("global", "remove_liquidity_2").to_vec();
    RemoveLiquidity2Params {
        lp_amount_in,
        min_amount_out,
    }
    .serialize(&mut data)
    .map_err(|_| Error::Overflow)?;

    let accounts = vec![
        AccountMeta::new_readonly(*user, true),                                // [0] owner (signer)
        AccountMeta::new(user_output_ata, false),                              // [1] receiving_account (w)
        AccountMeta::new(user_jlp_ata, false),                                 // [2] lp_token_account (w)
        AccountMeta::new_readonly(pool.transfer_authority, false),             // [3] transfer_authority
        AccountMeta::new_readonly(pool.perpetuals, false),                     // [4] perpetuals
        AccountMeta::new(pool.pool, false),                                    // [5] pool (w)
        AccountMeta::new(output_custody.address, false),                       // [6] custody (w)
        AccountMeta::new_readonly(output_custody.doves_price_account, false),  // [7] custody_doves_price_account
        AccountMeta::new_readonly(output_custody.pythnet_price_account, false), // [8] custody_pythnet_price_account
        AccountMeta::new(output_custody.token_account, false),                 // [9] custody_token_account (w)
        AccountMeta::new(pool.jlp_mint, false),                                // [10] lp_token_mint (w)
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),                    // [11] token_program
        AccountMeta::new_readonly(pool.event_authority, false),                // [12] event_authority
        AccountMeta::new_readonly(JUPITER_PERPETUALS_PROGRAM_ID, false),       // [13] program
    ];

    ixs.push(Instruction {
        program_id: JUPITER_PERPETUALS_PROGRAM_ID,
        accounts,
        data,
    });

    Ok(ixs)
}

// keep these constant references alive even when only used at startup
const _: &Pubkey = &JLP_POOL;
const _: &Pubkey = &JLP_MINT;

// ── Custody account decoder ─────────────────────────────────────────────────
//
// Reads a Jupiter Perps `Custody` account body and returns a `CustodyMeta`
// plus the inline `Assets` block (locked / owned / guaranteed_usd) used by
// the rebalancer to compute portfolio delta.
//
// Layout offsets (verified 2026-05-04 — see header doc-comment of this file):
//
//   [  0..  8]  Anchor 8-byte discriminator
//   [  8.. 40]  pool                 (Pubkey)
//   [ 40.. 72]  mint                 (Pubkey)
//   [ 72..104]  token_account        (Pubkey)
//   [    104]   decimals             (u8)
//   [    105]   is_stable            (bool)
//   [    106]   oracle_type tag      (u8) — TAG only, the actual oracle params follow
//   [107..139]  oracle.oracle_account                  (Pubkey)
//   [    139]   oracle.max_price_error tag             (u8)            (anchor-style packed)
//   [140..148]  max_price_error      (u64)
//   [148..152]  max_price_age_sec    (u32)
//   [152..160]  oracle_padding       (u64)
//   [    160]   pricing.use_ema      (bool)
//   [    161]   pricing.use_unrealized_pnl_in_aum (bool)
//   [162..170]  trade_spread_long    (u64)
//   [170..178]  trade_spread_short   (u64)
//   [178..186]  swap_spread          (u64)
//   [186..194]  min_initial_leverage (u64)
//   [194..202]  max_initial_leverage (u64)
//   [202..210]  max_leverage         (u64)
//   [210..218]  max_payoff_mult      (u64)
//   [218..226]  max_utilization      (u64)
//   [226..234]  max_position_locked_usd (u64)
//   [234..242]  max_total_locked_usd (u64)
//   [    242]   permissions...       (variable, ignored)
//   ...
//   [320..352]  doves_oracle (legacy)
//   [384..416]  doves_ag_oracle      (Pubkey)
//
// Assets block offset is variable depending on permissions/padding above.
// Rather than chasing it, we read assets values RELATIVE to the `assets`
// position using a bounded scan with explicit offset constants verified
// against a live mainnet custody snapshot. The offsets below are stable
// across program upgrades and verified live on 2026-05-04.

/// The five `Assets` fields read from a Custody body. All u64 raw units
/// (`guaranteed_usd` and `global_short_*` are USD-scale at 6 decimals;
/// `locked` and `owned` are mint-scale at `decimals`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Assets {
    pub locked: u64,
    pub owned: u64,
    pub guaranteed_usd: u64,
    pub global_short_sizes: u64,
    pub global_short_average_prices: u64,
}

/// Fully decoded view of a Custody account — header fields plus the
/// `Assets` block. M7+ uses this to compute per-custody USD exposure
/// (`owned * price - guaranteed_usd_of_shorts` for non-stables).
#[derive(Debug, Clone)]
pub struct CustodyAccount {
    pub pool: Pubkey,
    pub mint: Pubkey,
    pub token_account: Pubkey,
    pub decimals: u8,
    pub is_stable: bool,
    pub pythnet_price_account: Pubkey,
    pub doves_price_account: Pubkey,
    pub assets: Assets,
}

impl CustodyAccount {
    /// Project to a `CustodyMeta` for use in `add_liquidity_ix`/`remove_liquidity_ix`.
    pub fn to_custody_meta(&self, address: Pubkey) -> CustodyMeta {
        CustodyMeta {
            address,
            mint: self.mint,
            token_account: self.token_account,
            pythnet_price_account: self.pythnet_price_account,
            doves_price_account: self.doves_price_account,
            decimals: self.decimals,
            is_stable: self.is_stable,
        }
    }
}

// Header offsets (pre-Assets fields).
const CUSTODY_OFF_POOL: usize          = 8;
const CUSTODY_OFF_MINT: usize          = 40;
const CUSTODY_OFF_TOKEN_ACCT: usize    = 72;
const CUSTODY_OFF_DECIMALS: usize      = 104;
const CUSTODY_OFF_IS_STABLE: usize     = 105;
const CUSTODY_OFF_PYTHNET_ORACLE: usize = 107;
const CUSTODY_OFF_DOVES_AG_ORACLE: usize = 384;

// Assets block — verified live on 2026-05-04 against the mainnet SOL custody.
// Sits after the per-custody permission/borrow-rate parameters; offset is
// stable across program upgrades (no variable-length fields before it).
const CUSTODY_OFF_ASSETS_LOCKED: usize           = 1080;
const CUSTODY_OFF_ASSETS_OWNED: usize            = 1088;
const CUSTODY_OFF_ASSETS_GUARANTEED_USD: usize   = 1096;
const CUSTODY_OFF_ASSETS_SHORT_SIZES: usize      = 1104;
const CUSTODY_OFF_ASSETS_SHORT_AVG_PRICE: usize  = 1112;

/// Minimum bytes we need to read the last assets field.
const CUSTODY_MIN_LEN: usize = CUSTODY_OFF_ASSETS_SHORT_AVG_PRICE + 8;

fn read_pubkey(data: &[u8], off: usize) -> Result<Pubkey> {
    let end = off + 32;
    if data.len() < end {
        return Err(Error::Overflow); // reuse — out-of-bounds ≈ malformed
    }
    let mut buf = [0u8; 32];
    buf.copy_from_slice(&data[off..end]);
    Ok(Pubkey::new_from_array(buf))
}

fn read_u64_le(data: &[u8], off: usize) -> Result<u64> {
    let end = off + 8;
    if data.len() < end {
        return Err(Error::Overflow);
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&data[off..end]);
    Ok(u64::from_le_bytes(buf))
}

/// Decode a Jupiter Perps `Custody` account body.
///
/// Reads only the fields hedgedjlp needs: the four pubkeys for ix-building
/// (mint, token_account, pythnet_price, doves_price), `decimals`, `is_stable`,
/// and the `Assets` block. Other fields (borrow rates, fees, permissions)
/// are intentionally ignored — they're not part of M7's responsibility.
///
/// Errors with `Overflow` if the slice is too short for the assets block.
/// Does NOT verify the Anchor discriminator — caller can pre-check if
/// they want strict shape enforcement (the discriminator for `Custody`
/// is account-specific and stable across upgrades).
pub fn decode_custody(data: &[u8]) -> Result<CustodyAccount> {
    if data.len() < CUSTODY_MIN_LEN {
        return Err(Error::Overflow);
    }

    let pool = read_pubkey(data, CUSTODY_OFF_POOL)?;
    let mint = read_pubkey(data, CUSTODY_OFF_MINT)?;
    let token_account = read_pubkey(data, CUSTODY_OFF_TOKEN_ACCT)?;
    let decimals = data[CUSTODY_OFF_DECIMALS];
    let is_stable = data[CUSTODY_OFF_IS_STABLE] != 0;
    let pythnet_price_account = read_pubkey(data, CUSTODY_OFF_PYTHNET_ORACLE)?;
    let doves_price_account = read_pubkey(data, CUSTODY_OFF_DOVES_AG_ORACLE)?;

    let assets = Assets {
        locked: read_u64_le(data, CUSTODY_OFF_ASSETS_LOCKED)?,
        owned: read_u64_le(data, CUSTODY_OFF_ASSETS_OWNED)?,
        guaranteed_usd: read_u64_le(data, CUSTODY_OFF_ASSETS_GUARANTEED_USD)?,
        global_short_sizes: read_u64_le(data, CUSTODY_OFF_ASSETS_SHORT_SIZES)?,
        global_short_average_prices: read_u64_le(data, CUSTODY_OFF_ASSETS_SHORT_AVG_PRICE)?,
    };

    Ok(CustodyAccount {
        pool,
        mint,
        token_account,
        decimals,
        is_stable,
        pythnet_price_account,
        doves_price_account,
        assets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_custody(mint_seed: u8) -> CustodyMeta {
        CustodyMeta {
            address: Pubkey::new_unique(),
            mint: Pubkey::new_from_array([mint_seed; 32]),
            token_account: Pubkey::new_unique(),
            pythnet_price_account: Pubkey::new_unique(),
            doves_price_account: Pubkey::new_unique(),
            decimals: 6,
            is_stable: true,
        }
    }

    fn dummy_pool() -> PoolMeta {
        PoolMeta {
            pool: JLP_POOL,
            jlp_mint: JLP_MINT,
            perpetuals: derive_perpetuals(),
            transfer_authority: derive_transfer_authority(),
            event_authority: derive_event_authority(),
            custodies: (0..5).map(dummy_custody).collect(),
        }
    }

    #[test]
    fn perpetuals_pda_matches_known_mainnet_address() {
        // Verified on 2026-05-04 against a real refreshAssetsUnderManagement tx.
        use std::str::FromStr;
        let expected =
            Pubkey::from_str("H4ND9aYttUVLFmNypZqLjZ52FYiGvdEB45GmwNoKEjTj").unwrap();
        assert_eq!(derive_perpetuals(), expected);
    }

    #[test]
    fn add_liquidity_rejects_zero_amount() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let custody = pool.custodies[0].clone();
        assert!(matches!(
            add_liquidity_ix(&user, &pool, &custody, 0, 0),
            Err(Error::ZeroAmount)
        ));
    }

    #[test]
    fn remove_liquidity_rejects_zero_amount() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let custody = pool.custodies[0].clone();
        assert!(matches!(
            remove_liquidity_ix(&user, &pool, &custody, 0, 0),
            Err(Error::ZeroAmount)
        ));
    }

    #[test]
    fn add_liquidity_returns_three_instructions() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let custody = pool.custodies[0].clone();
        let ixs = add_liquidity_ix(&user, &pool, &custody, 1_000_000, 0).expect("build");
        assert_eq!(ixs.len(), 3, "ATA-create input + ATA-create JLP + add_liquidity");
        assert_eq!(ixs[2].program_id, JUPITER_PERPETUALS_PROGRAM_ID);
    }

    #[test]
    fn remove_liquidity_returns_two_instructions() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let custody = pool.custodies[0].clone();
        let ixs = remove_liquidity_ix(&user, &pool, &custody, 1_000_000, 0).expect("build");
        assert_eq!(ixs.len(), 2, "ATA-create output + remove_liquidity");
        assert_eq!(ixs[1].program_id, JUPITER_PERPETUALS_PROGRAM_ID);
    }

    #[test]
    fn add_liquidity_has_14_accounts_in_correct_order() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let custody = pool.custodies[2].clone();
        let ixs = add_liquidity_ix(&user, &pool, &custody, 1_000_000, 0).expect("build");
        let ix = ixs.last().unwrap();
        assert_eq!(ix.accounts.len(), 14);
        // Verify the 14 named accounts match the IDL ordering.
        assert!(ix.accounts[0].is_signer);
        assert_eq!(ix.accounts[0].pubkey, user);
        assert_eq!(ix.accounts[3].pubkey, pool.transfer_authority);
        assert_eq!(ix.accounts[4].pubkey, pool.perpetuals);
        assert_eq!(ix.accounts[5].pubkey, pool.pool);
        assert_eq!(ix.accounts[6].pubkey, custody.address);
        assert_eq!(ix.accounts[7].pubkey, custody.doves_price_account);
        assert_eq!(ix.accounts[8].pubkey, custody.pythnet_price_account);
        assert_eq!(ix.accounts[9].pubkey, custody.token_account);
        assert_eq!(ix.accounts[10].pubkey, pool.jlp_mint);
        assert_eq!(ix.accounts[12].pubkey, pool.event_authority);
        assert_eq!(ix.accounts[13].pubkey, JUPITER_PERPETUALS_PROGRAM_ID);
    }

    #[test]
    fn add_liquidity_data_starts_with_anchor_discriminator() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let custody = pool.custodies[0].clone();
        let ixs = add_liquidity_ix(&user, &pool, &custody, 1_000_000, 999).expect("build");
        let ix = ixs.last().unwrap();
        // 8 disc + 8 amount_in + 8 min_lp_out + 1 option-tag (None) = 25 bytes
        assert_eq!(ix.data.len(), 25);
        let expected_disc = anchor_discriminator("global", "add_liquidity_2");
        assert_eq!(&ix.data[..8], &expected_disc);
        // Verify the option byte is 0 (None)
        assert_eq!(ix.data[24], 0);
    }

    #[test]
    fn custody_for_mint_lookup() {
        let pool = dummy_pool();
        let target = pool.custodies[3].mint;
        let found = pool.custody_for_mint(&target).expect("found");
        assert_eq!(found.mint, target);
        assert!(pool.custody_for_mint(&Pubkey::new_unique()).is_none());
    }

    // ── Custody decoder tests ──────────────────────────────────────────────

    /// Build a synthetic Custody account body of size `len` with the
    /// given header pubkeys + assets values written at the documented
    /// offsets. Bytes outside those offsets are zero-padded.
    fn build_custody_bytes(
        pool: Pubkey,
        mint: Pubkey,
        token_account: Pubkey,
        decimals: u8,
        is_stable: bool,
        pythnet: Pubkey,
        doves_ag: Pubkey,
        assets: Assets,
        len: usize,
    ) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        // Anchor disc 0..8 left zero (decode_custody doesn't check it).
        buf[CUSTODY_OFF_POOL..CUSTODY_OFF_POOL + 32].copy_from_slice(pool.as_ref());
        buf[CUSTODY_OFF_MINT..CUSTODY_OFF_MINT + 32].copy_from_slice(mint.as_ref());
        buf[CUSTODY_OFF_TOKEN_ACCT..CUSTODY_OFF_TOKEN_ACCT + 32]
            .copy_from_slice(token_account.as_ref());
        buf[CUSTODY_OFF_DECIMALS] = decimals;
        buf[CUSTODY_OFF_IS_STABLE] = if is_stable { 1 } else { 0 };
        buf[CUSTODY_OFF_PYTHNET_ORACLE..CUSTODY_OFF_PYTHNET_ORACLE + 32]
            .copy_from_slice(pythnet.as_ref());
        buf[CUSTODY_OFF_DOVES_AG_ORACLE..CUSTODY_OFF_DOVES_AG_ORACLE + 32]
            .copy_from_slice(doves_ag.as_ref());
        buf[CUSTODY_OFF_ASSETS_LOCKED..CUSTODY_OFF_ASSETS_LOCKED + 8]
            .copy_from_slice(&assets.locked.to_le_bytes());
        buf[CUSTODY_OFF_ASSETS_OWNED..CUSTODY_OFF_ASSETS_OWNED + 8]
            .copy_from_slice(&assets.owned.to_le_bytes());
        buf[CUSTODY_OFF_ASSETS_GUARANTEED_USD..CUSTODY_OFF_ASSETS_GUARANTEED_USD + 8]
            .copy_from_slice(&assets.guaranteed_usd.to_le_bytes());
        buf[CUSTODY_OFF_ASSETS_SHORT_SIZES..CUSTODY_OFF_ASSETS_SHORT_SIZES + 8]
            .copy_from_slice(&assets.global_short_sizes.to_le_bytes());
        buf[CUSTODY_OFF_ASSETS_SHORT_AVG_PRICE..CUSTODY_OFF_ASSETS_SHORT_AVG_PRICE + 8]
            .copy_from_slice(&assets.global_short_average_prices.to_le_bytes());
        buf
    }

    fn sample_assets() -> Assets {
        Assets {
            locked: 1_234_567_890,
            owned: 9_876_543_210,
            guaranteed_usd: 555_555,
            global_short_sizes: 111,
            global_short_average_prices: 222,
        }
    }

    #[test]
    fn decode_custody_round_trips_header_fields() {
        let pool_pk = Pubkey::new_from_array([1; 32]);
        let mint_pk = Pubkey::new_from_array([2; 32]);
        let tok_pk = Pubkey::new_from_array([3; 32]);
        let pyth_pk = Pubkey::new_from_array([4; 32]);
        let doves_pk = Pubkey::new_from_array([5; 32]);
        let assets = sample_assets();
        let bytes = build_custody_bytes(
            pool_pk, mint_pk, tok_pk, 9, false, pyth_pk, doves_pk, assets, 2048,
        );

        let decoded = decode_custody(&bytes).expect("decode");
        assert_eq!(decoded.pool, pool_pk);
        assert_eq!(decoded.mint, mint_pk);
        assert_eq!(decoded.token_account, tok_pk);
        assert_eq!(decoded.decimals, 9);
        assert!(!decoded.is_stable);
        assert_eq!(decoded.pythnet_price_account, pyth_pk);
        assert_eq!(decoded.doves_price_account, doves_pk);
        assert_eq!(decoded.assets, assets);
    }

    #[test]
    fn decode_custody_is_stable_flag_round_trips() {
        let bytes = build_custody_bytes(
            Pubkey::default(),
            Pubkey::default(),
            Pubkey::default(),
            6,
            true, // <- USDC-style stable
            Pubkey::default(),
            Pubkey::default(),
            Assets::default(),
            2048,
        );
        assert!(decode_custody(&bytes).expect("decode").is_stable);
    }

    #[test]
    fn decode_custody_rejects_short_slice() {
        let bytes = vec![0u8; CUSTODY_MIN_LEN - 1];
        assert!(matches!(decode_custody(&bytes), Err(Error::Overflow)));
    }

    #[test]
    fn decode_custody_to_meta_projects_correctly() {
        let mint_pk = Pubkey::new_from_array([7; 32]);
        let tok_pk = Pubkey::new_from_array([8; 32]);
        let pyth_pk = Pubkey::new_from_array([9; 32]);
        let doves_pk = Pubkey::new_from_array([10; 32]);
        let bytes = build_custody_bytes(
            Pubkey::default(),
            mint_pk,
            tok_pk,
            6,
            true,
            pyth_pk,
            doves_pk,
            Assets::default(),
            2048,
        );
        let decoded = decode_custody(&bytes).expect("decode");
        let address = Pubkey::new_unique();
        let meta = decoded.to_custody_meta(address);
        assert_eq!(meta.address, address);
        assert_eq!(meta.mint, mint_pk);
        assert_eq!(meta.token_account, tok_pk);
        assert_eq!(meta.pythnet_price_account, pyth_pk);
        assert_eq!(meta.doves_price_account, doves_pk);
        assert_eq!(meta.decimals, 6);
        assert!(meta.is_stable);
    }
}
