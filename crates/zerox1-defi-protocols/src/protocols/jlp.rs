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
use serde::{Deserialize, Serialize};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;

use crate::{
    constants::{
        ASSOCIATED_TOKEN_PROGRAM_ID, JLP_MINT, JLP_POOL, JUPITER_PERPETUALS_PROGRAM_ID,
        SYSTEM_PROGRAM_ID, TOKEN_PROGRAM_ID,
    },
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
        AccountMeta::new_readonly(*user, true), // [0] owner (signer, not writable)
        AccountMeta::new(user_input_ata, false), // [1] funding_account (w)
        AccountMeta::new(user_jlp_ata, false),  // [2] lp_token_account (w)
        AccountMeta::new_readonly(pool.transfer_authority, false), // [3] transfer_authority
        AccountMeta::new_readonly(pool.perpetuals, false), // [4] perpetuals
        AccountMeta::new(pool.pool, false),     // [5] pool (w)
        AccountMeta::new(input_custody.address, false), // [6] custody (w)
        AccountMeta::new_readonly(input_custody.doves_price_account, false), // [7] custody_doves_price_account
        AccountMeta::new_readonly(input_custody.pythnet_price_account, false), // [8] custody_pythnet_price_account
        AccountMeta::new(input_custody.token_account, false), // [9] custody_token_account (w)
        AccountMeta::new(pool.jlp_mint, false),               // [10] lp_token_mint (w)
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),   // [11] token_program
        AccountMeta::new_readonly(pool.event_authority, false), // [12] event_authority
        AccountMeta::new_readonly(JUPITER_PERPETUALS_PROGRAM_ID, false), // [13] program (self)
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
        AccountMeta::new_readonly(*user, true),   // [0] owner (signer)
        AccountMeta::new(user_output_ata, false), // [1] receiving_account (w)
        AccountMeta::new(user_jlp_ata, false),    // [2] lp_token_account (w)
        AccountMeta::new_readonly(pool.transfer_authority, false), // [3] transfer_authority
        AccountMeta::new_readonly(pool.perpetuals, false), // [4] perpetuals
        AccountMeta::new(pool.pool, false),       // [5] pool (w)
        AccountMeta::new(output_custody.address, false), // [6] custody (w)
        AccountMeta::new_readonly(output_custody.doves_price_account, false), // [7] custody_doves_price_account
        AccountMeta::new_readonly(output_custody.pythnet_price_account, false), // [8] custody_pythnet_price_account
        AccountMeta::new(output_custody.token_account, false), // [9] custody_token_account (w)
        AccountMeta::new(pool.jlp_mint, false),                // [10] lp_token_mint (w)
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),    // [11] token_program
        AccountMeta::new_readonly(pool.event_authority, false), // [12] event_authority
        AccountMeta::new_readonly(JUPITER_PERPETUALS_PROGRAM_ID, false), // [13] program
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

/// The six `Assets` fields read from a Custody body, per IDL field
/// order (spec §6.1): `feesReserves, owned, locked, guaranteedUsd,
/// globalShortSizes, globalShortAveragePrices`. All u64. Audit fix 6.
///
/// `guaranteed_usd` and `global_short_*` are USD-scale at 6 decimals;
/// `fees_reserves`, `owned`, `locked` are mint-scale at `decimals`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Assets {
    pub fees_reserves: u64,
    pub owned: u64,
    pub locked: u64,
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
    /// `FundingRateState.hourlyFundingDbps` per spec §6.2 — audit fix 7.
    pub hourly_funding_dbps: u64,
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

    /// Hourly funding rate in *bps* per the IDL `FundingRateState`
    /// (`hourlyFundingDbps`, decimal-bps). Audit fix 7 / spec §6.2.
    /// 10 dbps = 1 bps; return saturates at u16::MAX so callers can
    /// compare directly against the assignment's
    /// `max_borrow_rate_bps` cap. Annualised rate = hourly * 8760.
    pub fn hourly_funding_bps(&self) -> u16 {
        let bps = self.hourly_funding_dbps / 10;
        bps.min(u16::MAX as u64) as u16
    }
}

/// Free-fn variant for the rebalancer: decode the
/// `FundingRateState.hourlyFundingDbps` field straight from raw bytes
/// and convert to bps. Returns `None` only when the slice is too short
/// to decode. Audit fix 7 / spec §6.2.
pub fn decode_custody_borrow_rate_bps(data: &[u8]) -> Option<u16> {
    if data.len() < CUSTODY_OFF_FUNDING_HOURLY_DBPS + 8 {
        return None;
    }
    let dbps = read_u64_le(data, CUSTODY_OFF_FUNDING_HOURLY_DBPS).ok()?;
    let bps = dbps / 10;
    Some(bps.min(u16::MAX as u64) as u16)
}

// Header offsets (pre-Assets fields).
const CUSTODY_OFF_POOL: usize = 8;
const CUSTODY_OFF_MINT: usize = 40;
const CUSTODY_OFF_TOKEN_ACCT: usize = 72;
const CUSTODY_OFF_DECIMALS: usize = 104;
const CUSTODY_OFF_IS_STABLE: usize = 105;
const CUSTODY_OFF_PYTHNET_ORACLE: usize = 107;
const CUSTODY_OFF_DOVES_AG_ORACLE: usize = 384;

// Assets block — IDL order per spec §6.1:
//   feesReserves, owned, locked, guaranteedUsd, globalShortSizes,
//   globalShortAveragePrices.
//
// Verified live on 2026-05-04 against the mainnet SOL custody: the
// block begins at offset 1080. Audit fix 6 corrects the field
// labeling — `feesReserves` is at the START of the block, not
// `locked`.
const CUSTODY_OFF_ASSETS_BASE: usize = 1080;
const CUSTODY_OFF_ASSETS_FEES_RESERVES: usize = CUSTODY_OFF_ASSETS_BASE;
const CUSTODY_OFF_ASSETS_OWNED: usize = CUSTODY_OFF_ASSETS_BASE + 8;
const CUSTODY_OFF_ASSETS_LOCKED: usize = CUSTODY_OFF_ASSETS_BASE + 16;
const CUSTODY_OFF_ASSETS_GUARANTEED_USD: usize = CUSTODY_OFF_ASSETS_BASE + 24;
const CUSTODY_OFF_ASSETS_SHORT_SIZES: usize = CUSTODY_OFF_ASSETS_BASE + 32;
const CUSTODY_OFF_ASSETS_SHORT_AVG_PRICE: usize = CUSTODY_OFF_ASSETS_BASE + 40;

// FundingRateState block (spec §6.2). Sits immediately after the Assets
// block in the IDL. Layout:
//   cumulativeInterestRate: u128 (16 bytes)
//   lastUpdate:             i64  (8 bytes)
//   hourlyFundingDbps:      u64  (8 bytes)
//
// Hourly funding decimal-bps: 10 dbps == 1 bps. To annualize: bps * 8760.
const CUSTODY_OFF_FUNDING_RATE_BASE: usize = CUSTODY_OFF_ASSETS_BASE + 48;
const CUSTODY_OFF_FUNDING_HOURLY_DBPS: usize = CUSTODY_OFF_FUNDING_RATE_BASE + 16 + 8;

/// Minimum bytes we need to read the last assets field.
const CUSTODY_MIN_LEN: usize = CUSTODY_OFF_FUNDING_HOURLY_DBPS + 8;

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
        fees_reserves: read_u64_le(data, CUSTODY_OFF_ASSETS_FEES_RESERVES)?,
        owned: read_u64_le(data, CUSTODY_OFF_ASSETS_OWNED)?,
        locked: read_u64_le(data, CUSTODY_OFF_ASSETS_LOCKED)?,
        guaranteed_usd: read_u64_le(data, CUSTODY_OFF_ASSETS_GUARANTEED_USD)?,
        global_short_sizes: read_u64_le(data, CUSTODY_OFF_ASSETS_SHORT_SIZES)?,
        global_short_average_prices: read_u64_le(data, CUSTODY_OFF_ASSETS_SHORT_AVG_PRICE)?,
    };

    let hourly_funding_dbps = read_u64_le(data, CUSTODY_OFF_FUNDING_HOURLY_DBPS)?;

    Ok(CustodyAccount {
        pool,
        mint,
        token_account,
        decimals,
        is_stable,
        pythnet_price_account,
        doves_price_account,
        assets,
        hourly_funding_dbps,
    })
}

// ── Jupiter Perps perp trading: 2-tx request-execute model ──────────────────
//
// Jupiter Perps separates a perp-position open into two transactions:
//
//   1. User submits `create_increase_position_request` with the desired
//      side / size / collateral. The on-chain program creates a
//      `PositionRequest` PDA holding the parameters.
//   2. An off-chain Jupiter keeper picks up the request 1-3 slots later
//      and calls `execute_increase_position_request`, which actually
//      moves the price into the position, debits collateral, and
//      either creates or increases the user's `Position` PDA.
//
// The hedgedjlp daemon only owns step 1 — submitting the request. The
// keeper handles step 2. M8 ships the open path; the close path
// (`create_decrease_position_request`) lands in M11.
//
// ## IDL-decode notes (MEDIUM CONFIDENCE)
//
// Jupiter Perps' Anchor IDL has not been formally re-verified against
// a live mainnet account dump as part of M8 — the discriminator,
// account ordering, and arg layout below are derived from the public
// references cited in the M8 plan:
//
//   - https://github.com/julianfssen/jupiter-perps-anchor-idl-parsing
//   - https://github.com/Garrett-Weber/jupiter-perpetuals-cpi
//
// The 8-byte discriminator is computed by the standard Anchor rule
// `sha256("global:create_increase_position_request_v2")[0..8]`. If the
// program is on a different ix name (e.g. without the `_v2` suffix),
// simulation will return InstructionError(InvalidInstructionData) and
// the daemon will surface error_code=5. M9's keeper-poll wires the
// observation that lets us catch and adjust the discriminator if the
// devnet smoke is still misnamed.
//
// **Confidence**: discriminator + side + arg layout are best-effort.
// Account ordering matches the IDL examples. Field count + ordering
// inside `PositionRequest` is conservative — we decode only the
// fields hedgedjlp needs (owner, position, side, size_usd_delta,
// collateral_token_delta).

/// Side of a Jupiter Perps perp position.
///
/// `Long` = bet on the asset rising; `Short` = bet against it. The
/// hedgedjlp daemon always opens `Short` to neutralize JLP's natural
/// long bias from holding wSOL/wETH/wBTC custody balances.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PerpSide {
    Long,
    Short,
}

impl PerpSide {
    /// On-chain wire byte. Per the Jupiter Perps IDL `Side` enum,
    /// declared as `{ None = 0, Long = 1, Short = 2 }` (see
    /// `jupiter-perps-bundle-spec.md` §3.4). This value is used
    /// BOTH as the params byte AND as the trailing seed byte of the
    /// Position PDA (§3.5). Audit fix 2/6.
    pub fn as_u8(self) -> u8 {
        match self {
            PerpSide::Long => 1,
            PerpSide::Short => 2,
        }
    }
}

/// The `request_change` enum byte used as the trailing seed of the
/// `PositionRequest` PDA (`jupiter-perps-bundle-spec.md` §3.6). Anchor
/// declaration order: `None = 0, Increase = 1, Decrease = 2`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestChange {
    Increase,
    Decrease,
}

impl RequestChange {
    pub fn as_u8(self) -> u8 {
        match self {
            RequestChange::Increase => 1,
            RequestChange::Decrease => 2,
        }
    }
}

/// Anchor-serialized arguments for `create_increase_position_market_request`.
///
/// Per the Jupiter Perps IDL (`jupiter-perps-bundle-spec.md` §3): 6
/// fields, with `side: Side` (u8 enum, 1=Long 2=Short — see
/// `PerpSide::as_u8`) and `jupiter_minimum_out: Option<u64>`. The
/// historical 11-field `_v2` shape is gone. Audit fixes 1, 4.
///
/// `price_slippage` is a 6-decimal USD price (the worst acceptable
/// mark price at execution time), NOT bps — see audit fix 8.
#[derive(BorshSerialize)]
struct CreateIncreasePositionMarketRequestParams {
    size_usd_delta: u64,
    collateral_token_delta: u64,
    side: u8,
    price_slippage: u64,
    jupiter_minimum_out: Option<u64>,
    counter: u64,
}

/// Build the instruction sequence that submits a request to OPEN
/// (or increase) a Jupiter Perps perp position.
///
/// Returns:
/// 1. Idempotent ATA-create for the user's collateral ATA (USDC for
///    the daemon's path)
/// 2. The `create_increase_position_request_v2` instruction
///
/// Caller is responsible for adding compute budget instructions and
/// for ensuring the wallet has sufficient collateral balance. The
/// off-chain keeper takes 1-3 slots to execute; the request account
/// is closed when execute fires.
///
/// **Note**: the Position and PositionRequest PDAs are NOT included
/// in `accounts` because the program derives them on-chain from
/// `(payer, custody, collateral_custody, side, counter)`. The
/// account list below matches the IDL example for the v2 ix.
///
/// **Confidence**: discriminator + account ordering best-effort —
/// see the module-level note above. Whitelist verify will pass
/// (program ID is JUPITER_PERPETUALS_PROGRAM_ID); simulation will
/// either succeed or return InstructionError, which the daemon
/// surfaces as `error_code=5`.
#[allow(clippy::too_many_arguments)]
pub fn create_increase_position_request_ix(
    payer: &Pubkey,
    pool: &PoolMeta,
    position_custody: &CustodyMeta,
    collateral_custody: &CustodyMeta,
    position: &Pubkey,
    position_request: &Pubkey,
    position_size_usd: u64,
    collateral_amount: u64,
    side: PerpSide,
    // Audit fix 8: worst acceptable mark price at execution, in
    // 6-decimal USD scale (NOT bps). For a Short open, pass
    // `mark_price * (1 + slippage_bps/10_000)`; for a Long open,
    // `mark_price * (1 - slippage_bps/10_000)`.
    price_slippage_micro_usd: u64,
    counter: u64,
) -> Result<Vec<Instruction>> {
    if position_size_usd == 0 || collateral_amount == 0 {
        return Err(Error::ZeroAmount);
    }

    let user_collateral_ata = ata(payer, &collateral_custody.mint);
    let position_request_ata = ata(position_request, &collateral_custody.mint);

    let mut ixs = Vec::with_capacity(2);

    // Ensure the payer's collateral ATA exists. The position_request_ata
    // is created by the perps program on-chain (Anchor `init` constraint),
    // so we don't pre-create it here.
    ixs.push(create_associated_token_account_idempotent(
        payer,
        payer,
        &collateral_custody.mint,
        &TOKEN_PROGRAM_ID,
    ));

    // Audit fix 1: correct ix name per IDL (spec §3). The historical
    // `_v2` variant does not exist in the deployed program.
    let mut data =
        anchor_discriminator("global", "create_increase_position_market_request").to_vec();
    CreateIncreasePositionMarketRequestParams {
        size_usd_delta: position_size_usd,
        collateral_token_delta: collateral_amount,
        side: side.as_u8(),
        price_slippage: price_slippage_micro_usd,
        // Audit fix 4: explicit Option-tag encoding. None = single tag byte.
        jupiter_minimum_out: None,
        counter,
    }
    .serialize(&mut data)
    .map_err(|_| Error::Overflow)?;

    // Account ordering follows the IDL example:
    //   [ 0] owner                       (signer, writable)
    //   [ 1] funding_account             (writable)  user's collateral ATA
    //   [ 2] perpetuals                  (readonly)
    //   [ 3] pool                        (writable)
    //   [ 4] position                    (writable)  position PDA
    //   [ 5] position_request            (writable)  request PDA (init by program)
    //   [ 6] position_request_ata        (writable)  ATA owned by request PDA
    //   [ 7] custody                     (readonly)  position-asset custody
    //   [ 8] collateral_custody          (readonly)  USDC custody
    //   [ 9] input_mint                  (readonly)  collateral mint
    //   [10] referral                    (readonly)  zeroed (no referral)
    //   [11] token_program               (readonly)
    //   [12] associated_token_program    (readonly)
    //   [13] system_program              (readonly)
    //   [14] event_authority             (readonly)
    //   [15] program                     (readonly)
    // Audit fix 5: Anchor's `Option<Account>` "None" sentinel is the
    // program's own pubkey, NOT `Pubkey::default()`. The latter triggers
    // `AccountNotInitialized` during account validation. Spec §3.
    let accounts = vec![
        AccountMeta::new(*payer, true),                               // [ 0]
        AccountMeta::new(user_collateral_ata, false),                 // [ 1]
        AccountMeta::new_readonly(pool.perpetuals, false),            // [ 2]
        AccountMeta::new_readonly(pool.pool, false),                  // [ 3] readonly per spec §3
        AccountMeta::new(*position, false),                           // [ 4]
        AccountMeta::new(*position_request, false),                   // [ 5]
        AccountMeta::new(position_request_ata, false),                // [ 6]
        AccountMeta::new_readonly(position_custody.address, false),   // [ 7]
        AccountMeta::new_readonly(collateral_custody.address, false), // [ 8]
        AccountMeta::new_readonly(collateral_custody.mint, false),    // [ 9]
        AccountMeta::new_readonly(JUPITER_PERPETUALS_PROGRAM_ID, false), // [10] referral=None (Anchor sentinel)
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),              // [11]
        AccountMeta::new_readonly(ASSOCIATED_TOKEN_PROGRAM_ID, false),   // [12]
        AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),             // [13]
        AccountMeta::new_readonly(pool.event_authority, false),          // [14]
        AccountMeta::new_readonly(JUPITER_PERPETUALS_PROGRAM_ID, false), // [15]
    ];

    ixs.push(Instruction {
        program_id: JUPITER_PERPETUALS_PROGRAM_ID,
        accounts,
        data,
    });

    Ok(ixs)
}

/// Anchor-serialized arguments for `create_decrease_position_request_v2`.
///
/// Field ordering and names are derived from the same public IDL parser
/// references cited above for the increase variant. The decrease ix
/// shape mirrors the increase ix closely — same slippage / counter /
/// trigger structure — but with `collateral_usd_delta` replacing
/// `collateral_token_delta` (the program denominates collateral
/// withdrawal in USD when closing) and an additional `entire_position`
/// flag that the daemon sets to 1 for full closes.
///
/// **Confidence**: layout is best-effort per the public IDL examples,
/// not verified against a live mainnet decrease-request encoding.
/// Same caveats as the increase variant — sim will surface
/// InstructionError if the encoding is off and the daemon emits
/// `error_code=5`.
/// Params for `create_decrease_position_market_request` per spec §4.
///
/// Field order **collateral first, size second** (opposite of increase).
/// 6 fields total. Audit fixes 1, 4.
#[derive(BorshSerialize)]
struct CreateDecreasePositionMarketRequestParams {
    collateral_usd_delta: u64,
    size_usd_delta: u64,
    price_slippage: u64,
    jupiter_minimum_out: Option<u64>,
    entire_position: Option<bool>,
    counter: u64,
}

/// Build the instruction sequence that submits a request to DECREASE
/// (close or partially close) a Jupiter Perps perp position.
///
/// Symmetric to `create_increase_position_request_ix`. Returns:
/// 1. Idempotent ATA-create for the `receive_mint` ATA on the payer
///    (so the keeper-execute step can deposit the released collateral
///    + PnL into a destination account that already exists)
/// 2. The `create_decrease_position_request_v2` instruction
///
/// For a full close, pass `size_to_decrease_usd = position.size_usd`
/// and `entire_position = true` is implied by the helper. For a
/// partial close, pass the desired notional reduction.
///
/// `receive_mint` selects which token the released collateral + PnL
/// gets paid out as. Typically USDC (matches the hedgedjlp collateral).
///
/// Caller is responsible for:
/// - adding compute budget instructions (RpcContext does this)
/// - ensuring `position` is the existing Position PDA (use
///   `derive_position` to compute it)
/// - choosing a unique `counter` (unix-seconds + per-asset offset
///   matches the increase-request convention)
///
/// **Note**: like the increase variant, the Position and
/// PositionRequest PDAs are NOT included in `accounts` because the
/// program derives them on-chain from `(payer, custody,
/// collateral_custody, side, counter)`. Account list below matches
/// the IDL example for the v2 ix.
///
/// **Confidence**: discriminator + account ordering + arg layout
/// best-effort per the public IDL examples cited in the increase
/// variant's docstring. Sim will surface InstructionError if any
/// piece is off; daemon emits `error_code=5`.
#[allow(clippy::too_many_arguments)]
pub fn create_decrease_position_request_ix(
    payer: &Pubkey,
    pool: &PoolMeta,
    position_custody: &CustodyMeta,
    collateral_custody: &CustodyMeta,
    position: &Pubkey,
    position_request: &Pubkey,
    receive_mint: &Pubkey,
    size_to_decrease_usd: u64,
    // Audit fix 8: 6-decimal USD slippage price. For a Short close,
    // pass `mark_price * (1 - slippage_bps/10_000)` (lower price =
    // better for the short).
    price_slippage_micro_usd: u64,
    counter: u64,
    entire_position: bool,
) -> Result<Vec<Instruction>> {
    // For full closes the keeper reads `entire_position` and ignores
    // size, so zero size is valid when `entire_position = true` (spec §4).
    if size_to_decrease_usd == 0 && !entire_position {
        return Err(Error::ZeroAmount);
    }

    let user_receive_ata = ata(payer, receive_mint);
    let position_request_ata = ata(position_request, receive_mint);

    let mut ixs = Vec::with_capacity(2);

    ixs.push(create_associated_token_account_idempotent(
        payer,
        payer,
        receive_mint,
        &TOKEN_PROGRAM_ID,
    ));

    // Audit fix 1: correct ix name per IDL (spec §4).
    let mut data =
        anchor_discriminator("global", "create_decrease_position_market_request").to_vec();
    CreateDecreasePositionMarketRequestParams {
        // Audit fix 4: collateral first, size second.
        collateral_usd_delta: 0,
        size_usd_delta: size_to_decrease_usd,
        price_slippage: price_slippage_micro_usd,
        jupiter_minimum_out: None,
        entire_position: Some(entire_position),
        counter,
    }
    .serialize(&mut data)
    .map_err(|_| Error::Overflow)?;

    // Audit fix 5: referral=None via Anchor sentinel (program self).
    let accounts = vec![
        AccountMeta::new(*payer, true),                    // [ 0] owner
        AccountMeta::new(user_receive_ata, false),         // [ 1] receiving_account
        AccountMeta::new_readonly(pool.perpetuals, false), // [ 2] perpetuals
        AccountMeta::new_readonly(pool.pool, false),       // [ 3] pool readonly (spec §4)
        AccountMeta::new_readonly(*position, false),       // [ 4] position readonly (spec §4)
        AccountMeta::new(*position_request, false),        // [ 5] position_request
        AccountMeta::new(position_request_ata, false),     // [ 6] position_request_ata
        AccountMeta::new_readonly(position_custody.address, false), // [ 7]
        AccountMeta::new_readonly(collateral_custody.address, false), // [ 8]
        AccountMeta::new_readonly(*receive_mint, false),   // [ 9] desired_mint
        AccountMeta::new_readonly(JUPITER_PERPETUALS_PROGRAM_ID, false), // [10] referral=None
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false), // [11]
        AccountMeta::new_readonly(ASSOCIATED_TOKEN_PROGRAM_ID, false), // [12]
        AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false), // [13]
        AccountMeta::new_readonly(pool.event_authority, false), // [14]
        AccountMeta::new_readonly(JUPITER_PERPETUALS_PROGRAM_ID, false), // [15]
    ];

    ixs.push(Instruction {
        program_id: JUPITER_PERPETUALS_PROGRAM_ID,
        accounts,
        data,
    });

    Ok(ixs)
}

/// Derive the canonical Position PDA for a given (owner, pool, custody,
/// collateral_custody, side) tuple. Seeds verified against the public
/// IDL examples; if the program uses a different seed list, the
/// AccountResolution constraint will fail at sim time and the daemon
/// surfaces error_code=5.
pub fn derive_position(
    owner: &Pubkey,
    pool: &Pubkey,
    custody: &Pubkey,
    collateral_custody: &Pubkey,
    side: PerpSide,
) -> Pubkey {
    let side_byte = [side.as_u8()];
    Pubkey::find_program_address(
        &[
            b"position",
            owner.as_ref(),
            pool.as_ref(),
            custody.as_ref(),
            collateral_custody.as_ref(),
            &side_byte,
        ],
        &JUPITER_PERPETUALS_PROGRAM_ID,
    )
    .0
}

/// Derive the PositionRequest PDA per spec §3.6 — FOUR seed slices:
/// `["position_request", position, counter_le, [request_change_byte]]`.
/// Audit fix 3.
///
/// `request_change` is `Increase` for open requests, `Decrease` for
/// close requests. A keeper executing a request derives the PDA from
/// on-chain state including `request_change` — a missing byte means
/// the daemon's request lands at a PDA no keeper will look at.
pub fn derive_position_request(
    position: &Pubkey,
    counter: u64,
    request_change: RequestChange,
) -> Pubkey {
    let counter_bytes = counter.to_le_bytes();
    let change_byte = [request_change.as_u8()];
    Pubkey::find_program_address(
        &[
            b"position_request",
            position.as_ref(),
            &counter_bytes,
            &change_byte,
        ],
        &JUPITER_PERPETUALS_PROGRAM_ID,
    )
    .0
}

/// Decoded view of a Jupiter Perps `PositionRequest` account body — only
/// the fields M9's keeper-poll needs to verify a request landed
/// on-chain. Full Position decoding is M11 work (close path).
///
/// Layout offsets are best-effort per the public IDL examples and have
/// NOT been verified against a live mainnet PositionRequest body. M9
/// will pull a real one and lock these in. Expect adjustments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionRequest {
    pub owner: Pubkey,
    pub pool: Pubkey,
    pub custody: Pubkey,
    pub collateral_custody: Pubkey,
    pub size_usd_delta: u64,
    pub collateral_token_delta: u64,
    pub side: PerpSide,
    pub counter: u64,
}

// PositionRequest layout (best-effort; verify in M9):
//   [  0..  8]  Anchor 8-byte disc
//   [  8.. 40]  owner
//   [ 40.. 72]  pool
//   [ 72..104]  custody
//   [104..136]  collateral_custody
//   [136..168]  mint (input mint — collateral)
//   [168..176]  open_time (i64)
//   [176..184]  update_time (i64)
//   [184..192]  size_usd_delta (u64)
//   [192..200]  collateral_token_delta (u64)
//   [200..208]  request_change (variable; first byte = side after some flags)
//
// The decoder below reads the most stable subset (owner / pool / custody /
// collateral_custody / size / collateral / counter at a known offset).
// `side` is stored inside a packed enum that the IDL parser shows at
// offset ~208 after a few u8/bool flags; we read it from offset 209
// (best-effort; M9 to verify).
const POS_REQ_OFF_OWNER: usize = 8;
const POS_REQ_OFF_POOL: usize = 40;
const POS_REQ_OFF_CUSTODY: usize = 72;
const POS_REQ_OFF_COLL_CUSTODY: usize = 104;
const POS_REQ_OFF_SIZE_USD: usize = 184;
const POS_REQ_OFF_COLL_DELTA: usize = 192;
const POS_REQ_OFF_SIDE: usize = 209;
const POS_REQ_OFF_COUNTER: usize = 232;
const POS_REQ_MIN_LEN: usize = POS_REQ_OFF_COUNTER + 8;

/// Decode a Jupiter Perps `PositionRequest` account body. Returns
/// the subset of fields the daemon uses to confirm a request landed
/// on-chain. Errors with `Overflow` if the slice is too short.
///
/// **Confidence**: layout is best-effort per the public IDL examples.
/// M9's keeper-poll re-verifies against a live mainnet account.
pub fn decode_position_request(data: &[u8]) -> Result<PositionRequest> {
    if data.len() < POS_REQ_MIN_LEN {
        return Err(Error::Overflow);
    }
    let owner = read_pubkey(data, POS_REQ_OFF_OWNER)?;
    let pool = read_pubkey(data, POS_REQ_OFF_POOL)?;
    let custody = read_pubkey(data, POS_REQ_OFF_CUSTODY)?;
    let collateral_custody = read_pubkey(data, POS_REQ_OFF_COLL_CUSTODY)?;
    let size_usd_delta = read_u64_le(data, POS_REQ_OFF_SIZE_USD)?;
    let collateral_token_delta = read_u64_le(data, POS_REQ_OFF_COLL_DELTA)?;
    // IDL Side enum: 0=None, 1=Long, 2=Short. Unknown values default
    // to Short for backward compatibility with telemetry callers.
    let side = match data[POS_REQ_OFF_SIDE] {
        1 => PerpSide::Long,
        _ => PerpSide::Short,
    };
    let counter = read_u64_le(data, POS_REQ_OFF_COUNTER)?;

    Ok(PositionRequest {
        owner,
        pool,
        custody,
        collateral_custody,
        size_usd_delta,
        collateral_token_delta,
        side,
        counter,
    })
}

// ── Position account decoder ───────────────────────────────────────────────
//
// Layout verified 2026-05-15 against the IDL committed at
// https://raw.githubusercontent.com/julianfssen/jupiter-perps-anchor-idl-parsing/main/src/idl/jupiter-perpetuals-idl-json.json
// (cross-checked against the historical monakki/jup-perps-client@91cec1505a
// snapshot from 2026-03-25 — identical struct).
//
// Anchor account body = 8-byte discriminator + Borsh struct (declaration order,
// no padding). The Side enum serializes as a single u8 variant index.
//
//   [  0..  8]   discriminator   = sha256("account:Position")[..8]
//                                = aa bc 8f e4 7a 40 f7 d0
//   [  8.. 40]   owner                (Pubkey)
//   [ 40.. 72]   pool                 (Pubkey)
//   [ 72..104]   custody              (Pubkey)
//   [104..136]   collateral_custody   (Pubkey)
//   [136..144]   open_time            (i64)
//   [144..152]   update_time          (i64)
//   [    152]    side                 (Side enum: 0=None, 1=Long, 2=Short)
//   [153..161]   price                (u64, USD with 6 decimals)
//   [161..169]   size_usd             (u64, USD with 6 decimals)
//   [169..177]   collateral_usd       (u64, USD with 6 decimals)
//   [177..185]   realised_pnl_usd     (i64, USD with 6 decimals)
//   [185..201]   cumulative_interest_snapshot (u128)
//   [201..209]   locked_amount        (u64, mint-decimals)
//   [    209]    bump                 (u8)
// Total: 210 bytes.

/// Anchor `account:Position` discriminator
/// (`sha256("account:Position")[..8]`).
pub const POSITION_DISCRIMINATOR: [u8; 8] = [0xaa, 0xbc, 0x8f, 0xe4, 0x7a, 0x40, 0xf7, 0xd0];

const POS_OFF_OWNER: usize = 8;
const POS_OFF_POOL: usize = 40;
const POS_OFF_CUSTODY: usize = 72;
const POS_OFF_COLL_CUSTODY: usize = 104;
const POS_OFF_OPEN_TIME: usize = 136;
const POS_OFF_UPDATE_TIME: usize = 144;
const POS_OFF_SIDE: usize = 152;
const POS_OFF_PRICE: usize = 153;
const POS_OFF_SIZE_USD: usize = 161;
const POS_OFF_COLLATERAL_USD: usize = 169;
const POS_OFF_REALISED_PNL: usize = 177;
const POS_OFF_LOCKED_AMOUNT: usize = 201;
/// Minimum bytes needed to decode every Position field through
/// `locked_amount` inclusive (we don't read `bump`). 201 + 8 = 209.
pub const POSITION_MIN_LEN: usize = POS_OFF_LOCKED_AMOUNT + 8;
/// Total on-chain Position account body length (incl. bump).
pub const POSITION_TOTAL_LEN: usize = POSITION_MIN_LEN + 1;

/// Fully decoded view of a Jupiter Perps `Position` account.
///
/// All USD-scaled fields (`price`, `size_usd`, `collateral_usd`,
/// `realised_pnl_usd`) carry 6 decimals — the same convention used
/// throughout the JLP program and Jupiter front-end.
///
/// Used by `riskwatcher-daemon`'s `jupiter_perps_poller` to compute
/// per-position liquidation distance against the maintenance-margin
/// derived from `Custody.pricing.max_leverage`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedPosition {
    pub address: Pubkey,
    pub owner: Pubkey,
    pub pool: Pubkey,
    pub custody: Pubkey,
    pub collateral_custody: Pubkey,
    pub open_time: i64,
    pub update_time: i64,
    pub side: PerpSide,
    /// Entry price (USD, 6 decimals).
    pub price: u64,
    /// Position notional in USD (6 decimals).
    pub size_usd: u64,
    /// Collateral remaining in USD (6 decimals).
    pub collateral_usd: u64,
    /// Realised PnL since open (USD, 6 decimals).
    pub realised_pnl_usd: i64,
    /// Locked token amount in the position-asset custody (mint-decimals).
    pub locked_amount: u64,
}

impl DecodedPosition {
    /// `true` when the position carries no notional — i.e. the PDA
    /// exists on-chain but has been fully closed.
    pub fn is_empty(&self) -> bool {
        self.size_usd == 0
    }
}

fn read_i64_le(data: &[u8], off: usize) -> Result<i64> {
    let end = off + 8;
    if data.len() < end {
        return Err(Error::Overflow);
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&data[off..end]);
    Ok(i64::from_le_bytes(buf))
}

/// Decode a Jupiter Perps `Position` account body.
///
/// Strictly verifies the 8-byte Anchor discriminator. Returns an error
/// if the slice is shorter than [`POSITION_MIN_LEN`] or the
/// discriminator does not match [`POSITION_DISCRIMINATOR`].
///
/// `side` is read from the 1-byte enum variant index per the IDL:
/// `0 = None, 1 = Long, 2 = Short`. A `None` (size-zero placeholder)
/// position decodes as `PerpSide::Long` arbitrarily — callers should
/// inspect [`DecodedPosition::is_empty`] before acting on `side`.
pub fn decode_position(address: Pubkey, data: &[u8]) -> Result<DecodedPosition> {
    if data.len() < POSITION_MIN_LEN {
        return Err(Error::Overflow);
    }
    if data[..8] != POSITION_DISCRIMINATOR {
        return Err(Error::Overflow);
    }
    let owner = read_pubkey(data, POS_OFF_OWNER)?;
    let pool = read_pubkey(data, POS_OFF_POOL)?;
    let custody = read_pubkey(data, POS_OFF_CUSTODY)?;
    let collateral_custody = read_pubkey(data, POS_OFF_COLL_CUSTODY)?;
    let open_time = read_i64_le(data, POS_OFF_OPEN_TIME)?;
    let update_time = read_i64_le(data, POS_OFF_UPDATE_TIME)?;
    let side = match data[POS_OFF_SIDE] {
        2 => PerpSide::Short,
        // 0 = None (uninitialised slot) and 1 = Long both decode here;
        // size-zero (`is_empty`) callers must check before trusting side.
        _ => PerpSide::Long,
    };
    let price = read_u64_le(data, POS_OFF_PRICE)?;
    let size_usd = read_u64_le(data, POS_OFF_SIZE_USD)?;
    let collateral_usd = read_u64_le(data, POS_OFF_COLLATERAL_USD)?;
    let realised_pnl_usd = read_i64_le(data, POS_OFF_REALISED_PNL)?;
    let locked_amount = read_u64_le(data, POS_OFF_LOCKED_AMOUNT)?;

    Ok(DecodedPosition {
        address,
        owner,
        pool,
        custody,
        collateral_custody,
        open_time,
        update_time,
        side,
        price,
        size_usd,
        collateral_usd,
        realised_pnl_usd,
        locked_amount,
    })
}

// ── Custody.pricing.max_leverage ────────────────────────────────────────────
//
// `Custody.pricing` is `PricingParams` (IDL §6.3). Field layout, all u64:
//   trade_impact_fee_scalar, buffer, swap_spread, max_leverage,
//   max_global_long_sizes, max_global_short_sizes.
//
// The `pricing` block sits after the `oracle` block in the Custody body.
// `oracle` is `OracleParams { oracle_account: Pubkey, oracle_type: enum,
// buffer: u64, max_price_age_sec: u32 }` = 32 + 1 + 8 + 4 = 45 bytes,
// starting at offset 106 (see `CUSTODY_OFF_PYTHNET_ORACLE` minus 1 for
// the type tag).
//
// However, the existing custody decoder already verifies that the
// `Assets` block sits at offset 1080 — i.e. all the intermediate
// `oracle`/`pricing`/`permissions`/`target_ratio_bps` fields are
// stably-laid-out before that anchor. `max_leverage` is the 4th u64 in
// PricingParams (offset +24 from the block base). The pricing block
// itself begins right after `oracle` (45 bytes from `oracle.oracle_account`).
//
// Rather than chase 1080-back offsets that may rearrange across program
// upgrades, we look up `max_leverage` by scanning from a stable anchor.
// The pricing block immediately follows `oracle` (which ends at
// CUSTODY_OFF_PYTHNET_ORACLE - 1 + 1 + 32 + 1 + 8 + 4 = 152). So
// `max_leverage` sits at 152 + 24 = 176.
//
// Verified 2026-05-15 against mainnet SOL / BTC / ETH custody bodies:
// the u64 at offset 176 reads `19_531` for all three. With the
// thousandths scale (= leverage × 1_000), that resolves to **19.531×**
// max leverage — the cap the Jupiter front-end displays today, and the
// cap consistent with live hedgedjlp shorts running at ~5× without
// liquidation. See `docs/jupiter-perps-position-spec.md` §4.1 for the
// scale-interpretation correction (fleet-v0.2.7).
const CUSTODY_OFF_MAX_LEVERAGE: usize = 176;

/// Decode `Custody.pricing.max_leverage` from a raw Custody account
/// body. Returns `None` if the slice is too short.
///
/// **Scale**: the returned u64 is **leverage × 1_000** (thousandths).
/// E.g. `19_531` → 19.531× max leverage.
///
/// The function name retains the historical `_bps` suffix for
/// API stability; consumers (`riskwatcher_daemon::jupiter_perps_poller`)
/// treat it as `max_leverage_thousandths`. See
/// `docs/jupiter-perps-position-spec.md` §4.1.
pub fn decode_custody_max_leverage_bps(data: &[u8]) -> Option<u64> {
    if data.len() < CUSTODY_OFF_MAX_LEVERAGE + 8 {
        return None;
    }
    read_u64_le(data, CUSTODY_OFF_MAX_LEVERAGE).ok()
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
        let expected = Pubkey::from_str("H4ND9aYttUVLFmNypZqLjZ52FYiGvdEB45GmwNoKEjTj").unwrap();
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
        assert_eq!(
            ixs.len(),
            3,
            "ATA-create input + ATA-create JLP + add_liquidity"
        );
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
        build_custody_bytes_with_funding(
            pool,
            mint,
            token_account,
            decimals,
            is_stable,
            pythnet,
            doves_ag,
            assets,
            0,
            len,
        )
    }

    fn build_custody_bytes_with_funding(
        pool: Pubkey,
        mint: Pubkey,
        token_account: Pubkey,
        decimals: u8,
        is_stable: bool,
        pythnet: Pubkey,
        doves_ag: Pubkey,
        assets: Assets,
        hourly_funding_dbps: u64,
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
        // Assets block (IDL order, audit fix 6).
        buf[CUSTODY_OFF_ASSETS_FEES_RESERVES..CUSTODY_OFF_ASSETS_FEES_RESERVES + 8]
            .copy_from_slice(&assets.fees_reserves.to_le_bytes());
        buf[CUSTODY_OFF_ASSETS_OWNED..CUSTODY_OFF_ASSETS_OWNED + 8]
            .copy_from_slice(&assets.owned.to_le_bytes());
        buf[CUSTODY_OFF_ASSETS_LOCKED..CUSTODY_OFF_ASSETS_LOCKED + 8]
            .copy_from_slice(&assets.locked.to_le_bytes());
        buf[CUSTODY_OFF_ASSETS_GUARANTEED_USD..CUSTODY_OFF_ASSETS_GUARANTEED_USD + 8]
            .copy_from_slice(&assets.guaranteed_usd.to_le_bytes());
        buf[CUSTODY_OFF_ASSETS_SHORT_SIZES..CUSTODY_OFF_ASSETS_SHORT_SIZES + 8]
            .copy_from_slice(&assets.global_short_sizes.to_le_bytes());
        buf[CUSTODY_OFF_ASSETS_SHORT_AVG_PRICE..CUSTODY_OFF_ASSETS_SHORT_AVG_PRICE + 8]
            .copy_from_slice(&assets.global_short_average_prices.to_le_bytes());
        buf[CUSTODY_OFF_FUNDING_HOURLY_DBPS..CUSTODY_OFF_FUNDING_HOURLY_DBPS + 8]
            .copy_from_slice(&hourly_funding_dbps.to_le_bytes());
        buf
    }

    fn sample_assets() -> Assets {
        Assets {
            fees_reserves: 42_000,
            owned: 9_876_543_210,
            locked: 1_234_567_890,
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

    // ── Perp ixn-builder tests (M8) ────────────────────────────────────────

    #[test]
    fn perp_side_wire_bytes() {
        // Audit fix 2 / spec §3.4: IDL `Side` enum is
        // `None=0, Long=1, Short=2`. Used in BOTH the params byte AND
        // the Position PDA seed.
        assert_eq!(PerpSide::Long.as_u8(), 1);
        assert_eq!(PerpSide::Short.as_u8(), 2);
    }

    #[test]
    fn request_change_wire_bytes() {
        // Audit fix 3 / spec §3.6: Increase=1, Decrease=2.
        assert_eq!(RequestChange::Increase.as_u8(), 1);
        assert_eq!(RequestChange::Decrease.as_u8(), 2);
    }

    #[test]
    fn anchor_discriminator_for_market_request_matches_idl() {
        // Audit fix 1 / spec §3, §4: the ix names are
        // `create_increase_position_market_request` and
        // `create_decrease_position_market_request`. Pin the
        // discriminator preimage so any future rename surfaces here.
        let inc = anchor_discriminator("global", "create_increase_position_market_request");
        let dec = anchor_discriminator("global", "create_decrease_position_market_request");
        // Different preimages must yield different bytes.
        assert_ne!(inc, dec);
        // Stale `_v2` names must NOT match.
        let stale_inc = anchor_discriminator("global", "create_increase_position_request_v2");
        let stale_dec = anchor_discriminator("global", "create_decrease_position_request_v2");
        assert_ne!(inc, stale_inc);
        assert_ne!(dec, stale_dec);
    }

    #[test]
    fn position_request_pda_uses_four_seed_slices() {
        // Audit fix 3 / spec §3.6: PositionRequest PDA derives from 4
        // seed slices including the trailing request_change byte.
        // Increase vs Decrease against the same (position, counter)
        // MUST land at different PDAs.
        let position = Pubkey::new_unique();
        let inc = derive_position_request(&position, 7777, RequestChange::Increase);
        let dec = derive_position_request(&position, 7777, RequestChange::Decrease);
        assert_ne!(
            inc, dec,
            "Increase and Decrease must derive distinct PositionRequest PDAs"
        );
    }

    #[test]
    fn decode_custody_assets_field_order_matches_idl() {
        // Audit fix 6 / spec §6.1: IDL field order is
        // feesReserves, owned, locked, guaranteedUsd, globalShortSizes,
        // globalShortAveragePrices. Pin the round-trip so a future
        // offset slip surfaces here.
        let assets = Assets {
            fees_reserves: 1_111,
            owned: 2_222,
            locked: 3_333,
            guaranteed_usd: 4_444,
            global_short_sizes: 5_555,
            global_short_average_prices: 6_666,
        };
        let bytes = build_custody_bytes(
            Pubkey::default(),
            Pubkey::default(),
            Pubkey::default(),
            6,
            false,
            Pubkey::default(),
            Pubkey::default(),
            assets,
            2048,
        );
        let decoded = decode_custody(&bytes).expect("decode");
        assert_eq!(decoded.assets, assets);
    }

    #[test]
    fn decode_custody_borrow_rate_bps_reads_funding_state() {
        // Audit fix 7 / spec §6.2: `decode_custody_borrow_rate_bps`
        // returns Some(hourlyFundingDbps/10) — not unconditionally None
        // like the previous implementation.
        let dbps: u64 = 500; // 50 bps/h ≈ 4380% APR — sample only.
        let bytes = build_custody_bytes_with_funding(
            Pubkey::default(),
            Pubkey::default(),
            Pubkey::default(),
            6,
            false,
            Pubkey::default(),
            Pubkey::default(),
            Assets::default(),
            dbps,
            2048,
        );
        let bps = decode_custody_borrow_rate_bps(&bytes).expect("some bps");
        assert_eq!(bps, 50);
        // Short slice → still None.
        assert!(decode_custody_borrow_rate_bps(&[0u8; 100]).is_none());
    }

    #[test]
    fn create_increase_position_request_rejects_zero_size() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let pos_custody = pool.custodies[0].clone();
        let coll_custody = pool.custodies[1].clone();
        let position = Pubkey::new_unique();
        let req = Pubkey::new_unique();
        assert!(matches!(
            create_increase_position_request_ix(
                &user,
                &pool,
                &pos_custody,
                &coll_custody,
                &position,
                &req,
                0,
                100,
                PerpSide::Short,
                100_500_000,
                1,
            ),
            Err(Error::ZeroAmount)
        ));
    }

    #[test]
    fn create_increase_position_request_rejects_zero_collateral() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let pos_custody = pool.custodies[0].clone();
        let coll_custody = pool.custodies[1].clone();
        let position = Pubkey::new_unique();
        let req = Pubkey::new_unique();
        assert!(matches!(
            create_increase_position_request_ix(
                &user,
                &pool,
                &pos_custody,
                &coll_custody,
                &position,
                &req,
                100,
                0,
                PerpSide::Short,
                100_500_000,
                1,
            ),
            Err(Error::ZeroAmount)
        ));
    }

    #[test]
    fn create_increase_position_request_returns_two_ixns_with_correct_program() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let pos_custody = pool.custodies[0].clone();
        let coll_custody = pool.custodies[1].clone();
        let position = Pubkey::new_unique();
        let req = Pubkey::new_unique();
        let ixs = create_increase_position_request_ix(
            &user,
            &pool,
            &pos_custody,
            &coll_custody,
            &position,
            &req,
            10_000_000,
            5_000_000,
            PerpSide::Short,
            100_500_000,
            42,
        )
        .expect("build");
        assert_eq!(
            ixs.len(),
            2,
            "ATA-create + create_increase_position_market_request"
        );
        assert_eq!(ixs[1].program_id, JUPITER_PERPETUALS_PROGRAM_ID);
        assert_eq!(ixs[1].accounts.len(), 16);
    }

    #[test]
    fn create_increase_position_request_data_starts_with_anchor_disc() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let pos_custody = pool.custodies[0].clone();
        let coll_custody = pool.custodies[1].clone();
        let position = Pubkey::new_unique();
        let req = Pubkey::new_unique();
        let ixs = create_increase_position_request_ix(
            &user,
            &pool,
            &pos_custody,
            &coll_custody,
            &position,
            &req,
            10_000_000,
            5_000_000,
            PerpSide::Short,
            100_500_000,
            7,
        )
        .expect("build");
        let ix = ixs.last().unwrap();
        // Audit fix 1 / spec §3 — name is *_market_request.
        let expected = anchor_discriminator("global", "create_increase_position_market_request");
        assert_eq!(&ix.data[..8], &expected);
        // Params layout per spec §3:
        //   [ 0.. 8] size_usd_delta
        //   [ 8..16] collateral_token_delta
        //   [   16] side (1 = Long, 2 = Short)
        //   [17..25] price_slippage (u64 micro-USD)
        //   [   25] jupiter_minimum_out option tag (0 = None)
        //   [26..34] counter
        let body = &ix.data[8..];
        assert_eq!(body[16], PerpSide::Short.as_u8(), "side byte must be 2");
        assert_eq!(body[25], 0, "jupiter_minimum_out is None");
        assert_eq!(
            u64::from_le_bytes(body[26..34].try_into().unwrap()),
            7,
            "counter follows option tag"
        );
        // Total len = 8 disc + 8 + 8 + 1 + 8 + 1 + 8 = 42.
        assert_eq!(ix.data.len(), 42);
    }

    #[test]
    fn create_increase_position_request_signer_is_payer() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let pos_custody = pool.custodies[0].clone();
        let coll_custody = pool.custodies[1].clone();
        let position = Pubkey::new_unique();
        let req = Pubkey::new_unique();
        let ixs = create_increase_position_request_ix(
            &user,
            &pool,
            &pos_custody,
            &coll_custody,
            &position,
            &req,
            10_000_000,
            5_000_000,
            PerpSide::Short,
            100_500_000,
            1,
        )
        .expect("build");
        let ix = ixs.last().unwrap();
        assert!(ix.accounts[0].is_signer);
        assert_eq!(ix.accounts[0].pubkey, user);
        assert!(ix.accounts[0].is_writable);
        assert_eq!(ix.accounts[3].pubkey, pool.pool);
        assert!(!ix.accounts[3].is_writable, "pool readonly per spec §3");
        assert_eq!(ix.accounts[7].pubkey, pos_custody.address);
        assert_eq!(ix.accounts[8].pubkey, coll_custody.address);
        // Audit fix 5: referral=None is encoded as the program's own
        // pubkey, NOT Pubkey::default().
        assert_eq!(ix.accounts[10].pubkey, JUPITER_PERPETUALS_PROGRAM_ID);
        assert_ne!(ix.accounts[10].pubkey, Pubkey::default());
    }

    #[test]
    fn derive_position_distinguishes_sides() {
        // Long and Short should derive to different PDAs since side is
        // a seed component.
        let owner = Pubkey::new_unique();
        let pool = Pubkey::new_unique();
        let custody = Pubkey::new_unique();
        let coll = Pubkey::new_unique();
        let long = derive_position(&owner, &pool, &custody, &coll, PerpSide::Long);
        let short = derive_position(&owner, &pool, &custody, &coll, PerpSide::Short);
        assert_ne!(long, short);
    }

    #[test]
    fn derive_position_request_distinguishes_counters() {
        // Different counters → different request PDAs (allows concurrent
        // requests against same Position).
        let position = Pubkey::new_unique();
        let r1 = derive_position_request(&position, 1, RequestChange::Increase);
        let r2 = derive_position_request(&position, 2, RequestChange::Increase);
        assert_ne!(r1, r2);
    }

    #[test]
    fn decode_position_request_rejects_short_slice() {
        let bytes = vec![0u8; POS_REQ_MIN_LEN - 1];
        assert!(matches!(
            decode_position_request(&bytes),
            Err(Error::Overflow)
        ));
    }

    #[test]
    fn decode_position_request_round_trips_pubkeys_and_amounts() {
        let owner = Pubkey::new_from_array([1; 32]);
        let pool_pk = Pubkey::new_from_array([2; 32]);
        let custody_pk = Pubkey::new_from_array([3; 32]);
        let coll_pk = Pubkey::new_from_array([4; 32]);
        let mut buf = vec![0u8; POS_REQ_MIN_LEN + 16];
        buf[POS_REQ_OFF_OWNER..POS_REQ_OFF_OWNER + 32].copy_from_slice(owner.as_ref());
        buf[POS_REQ_OFF_POOL..POS_REQ_OFF_POOL + 32].copy_from_slice(pool_pk.as_ref());
        buf[POS_REQ_OFF_CUSTODY..POS_REQ_OFF_CUSTODY + 32].copy_from_slice(custody_pk.as_ref());
        buf[POS_REQ_OFF_COLL_CUSTODY..POS_REQ_OFF_COLL_CUSTODY + 32]
            .copy_from_slice(coll_pk.as_ref());
        buf[POS_REQ_OFF_SIZE_USD..POS_REQ_OFF_SIZE_USD + 8]
            .copy_from_slice(&123_456_u64.to_le_bytes());
        buf[POS_REQ_OFF_COLL_DELTA..POS_REQ_OFF_COLL_DELTA + 8]
            .copy_from_slice(&654_321_u64.to_le_bytes());
        buf[POS_REQ_OFF_SIDE] = 2; // Short (IDL Side enum, audit fix 2)
        buf[POS_REQ_OFF_COUNTER..POS_REQ_OFF_COUNTER + 8].copy_from_slice(&777_u64.to_le_bytes());

        let decoded = decode_position_request(&buf).expect("decode");
        assert_eq!(decoded.owner, owner);
        assert_eq!(decoded.pool, pool_pk);
        assert_eq!(decoded.custody, custody_pk);
        assert_eq!(decoded.collateral_custody, coll_pk);
        assert_eq!(decoded.size_usd_delta, 123_456);
        assert_eq!(decoded.collateral_token_delta, 654_321);
        assert_eq!(decoded.side, PerpSide::Short);
        assert_eq!(decoded.counter, 777);
    }

    // ── Perp close-request ixn-builder tests (M11) ────────────────────────

    #[test]
    fn create_decrease_position_request_rejects_zero_size_partial() {
        // For a partial close (entire_position=false), zero size is
        // invalid. For a full close (entire_position=true), zero size
        // is valid — keeper reads `entire_position` and ignores size.
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let pos_custody = pool.custodies[0].clone();
        let coll_custody = pool.custodies[1].clone();
        let position = Pubkey::new_unique();
        let req = Pubkey::new_unique();
        let receive_mint = Pubkey::new_unique();
        assert!(matches!(
            create_decrease_position_request_ix(
                &user,
                &pool,
                &pos_custody,
                &coll_custody,
                &position,
                &req,
                &receive_mint,
                0,
                90_500_000,
                7,
                false,
            ),
            Err(Error::ZeroAmount)
        ));
    }

    #[test]
    fn create_decrease_position_request_returns_two_ixns_with_correct_program() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let pos_custody = pool.custodies[0].clone();
        let coll_custody = pool.custodies[1].clone();
        let position = Pubkey::new_unique();
        let req = Pubkey::new_unique();
        let receive_mint = Pubkey::new_unique();
        let ixs = create_decrease_position_request_ix(
            &user,
            &pool,
            &pos_custody,
            &coll_custody,
            &position,
            &req,
            &receive_mint,
            10_000_000,
            90_500_000,
            42,
            true,
        )
        .expect("build");
        assert_eq!(
            ixs.len(),
            2,
            "ATA-create + create_decrease_position_market_request"
        );
        assert_eq!(ixs[1].program_id, JUPITER_PERPETUALS_PROGRAM_ID);
        assert_eq!(ixs[1].accounts.len(), 16);
    }

    #[test]
    fn create_decrease_position_request_data_starts_with_anchor_disc() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let pos_custody = pool.custodies[0].clone();
        let coll_custody = pool.custodies[1].clone();
        let position = Pubkey::new_unique();
        let req = Pubkey::new_unique();
        let receive_mint = Pubkey::new_unique();
        let ixs = create_decrease_position_request_ix(
            &user,
            &pool,
            &pos_custody,
            &coll_custody,
            &position,
            &req,
            &receive_mint,
            10_000_000,
            90_500_000,
            7,
            false,
        )
        .expect("build");
        let ix = ixs.last().unwrap();
        // Audit fix 1 / spec §4: ix is *_market_request.
        let expected = anchor_discriminator("global", "create_decrease_position_market_request");
        assert_eq!(&ix.data[..8], &expected);
    }

    #[test]
    fn create_decrease_position_request_params_layout_market_variant() {
        // Spec §4 Market variant params:
        //   [ 0.. 8] collateral_usd_delta
        //   [ 8..16] size_usd_delta            (collateral FIRST, audit fix 4)
        //   [16..24] price_slippage
        //   [   24] jupiter_minimum_out option tag
        //   [   25] entire_position option tag (1) + value byte (1)
        //   [27..35] counter
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let pos_custody = pool.custodies[0].clone();
        let coll_custody = pool.custodies[1].clone();
        let position = Pubkey::new_unique();
        let req = Pubkey::new_unique();
        let receive_mint = Pubkey::new_unique();

        let ixs_full = create_decrease_position_request_ix(
            &user,
            &pool,
            &pos_custody,
            &coll_custody,
            &position,
            &req,
            &receive_mint,
            10_000_000,
            90_500_000,
            1,
            true,
        )
        .expect("build");
        let body = &ixs_full[1].data[8..];
        // collateral_usd_delta (first) = 0
        assert_eq!(u64::from_le_bytes(body[0..8].try_into().unwrap()), 0);
        // size_usd_delta = 10_000_000
        assert_eq!(
            u64::from_le_bytes(body[8..16].try_into().unwrap()),
            10_000_000
        );
        assert_eq!(
            u64::from_le_bytes(body[16..24].try_into().unwrap()),
            90_500_000,
            "price_slippage is 6-decimal USD (audit fix 8)"
        );
        assert_eq!(body[24], 0, "jupiter_minimum_out=None");
        assert_eq!(body[25], 1, "entire_position option tag=Some");
        assert_eq!(body[26], 1, "entire_position value=true for full close");
        assert_eq!(u64::from_le_bytes(body[27..35].try_into().unwrap()), 1);

        let ixs_partial = create_decrease_position_request_ix(
            &user,
            &pool,
            &pos_custody,
            &coll_custody,
            &position,
            &req,
            &receive_mint,
            5_000_000,
            90_500_000,
            2,
            false,
        )
        .expect("build");
        assert_eq!(
            ixs_partial[1].data[8 + 26],
            0,
            "entire_position value=false for partial close"
        );
    }

    #[test]
    fn create_decrease_position_request_signer_is_payer() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let pos_custody = pool.custodies[0].clone();
        let coll_custody = pool.custodies[1].clone();
        let position = Pubkey::new_unique();
        let req = Pubkey::new_unique();
        let receive_mint = Pubkey::new_from_array([42; 32]);
        let ixs = create_decrease_position_request_ix(
            &user,
            &pool,
            &pos_custody,
            &coll_custody,
            &position,
            &req,
            &receive_mint,
            10_000_000,
            90_500_000,
            1,
            true,
        )
        .expect("build");
        let ix = ixs.last().unwrap();
        assert!(ix.accounts[0].is_signer);
        assert_eq!(ix.accounts[0].pubkey, user);
        assert_eq!(ix.accounts[3].pubkey, pool.pool);
        assert_eq!(ix.accounts[7].pubkey, pos_custody.address);
        assert_eq!(ix.accounts[8].pubkey, coll_custody.address);
        assert_eq!(ix.accounts[9].pubkey, receive_mint);
        // Audit fix 5: referral=None sentinel = program id.
        assert_eq!(ix.accounts[10].pubkey, JUPITER_PERPETUALS_PROGRAM_ID);
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

    // ── Position decoder tests ─────────────────────────────────────────────

    /// Build a canonical Position account body matching the IDL byte
    /// layout. Used by the decoder round-trip tests below — gives us a
    /// known-good fixture without needing a live mainnet snapshot.
    fn build_position_bytes(
        owner: Pubkey,
        pool: Pubkey,
        custody: Pubkey,
        collateral_custody: Pubkey,
        open_time: i64,
        update_time: i64,
        side_byte: u8,
        price: u64,
        size_usd: u64,
        collateral_usd: u64,
        realised_pnl_usd: i64,
        locked_amount: u64,
    ) -> Vec<u8> {
        let mut buf = vec![0u8; POSITION_TOTAL_LEN];
        buf[0..8].copy_from_slice(&POSITION_DISCRIMINATOR);
        buf[8..40].copy_from_slice(owner.as_ref());
        buf[40..72].copy_from_slice(pool.as_ref());
        buf[72..104].copy_from_slice(custody.as_ref());
        buf[104..136].copy_from_slice(collateral_custody.as_ref());
        buf[136..144].copy_from_slice(&open_time.to_le_bytes());
        buf[144..152].copy_from_slice(&update_time.to_le_bytes());
        buf[152] = side_byte;
        buf[153..161].copy_from_slice(&price.to_le_bytes());
        buf[161..169].copy_from_slice(&size_usd.to_le_bytes());
        buf[169..177].copy_from_slice(&collateral_usd.to_le_bytes());
        buf[177..185].copy_from_slice(&realised_pnl_usd.to_le_bytes());
        // cumulative_interest_snapshot at [185..201] left zero.
        buf[201..209].copy_from_slice(&locked_amount.to_le_bytes());
        buf[209] = 254; // bump
        buf
    }

    #[test]
    fn position_total_len_matches_idl() {
        // 8-byte disc + 14 fields = 210 bytes.
        assert_eq!(POSITION_TOTAL_LEN, 210);
    }

    #[test]
    fn decode_position_round_trips_all_fields() {
        let owner = Pubkey::new_from_array([1; 32]);
        let pool = Pubkey::new_from_array([2; 32]);
        let custody = Pubkey::new_from_array([3; 32]);
        let coll = Pubkey::new_from_array([4; 32]);
        let bytes = build_position_bytes(
            owner,
            pool,
            custody,
            coll,
            1_700_000_000,
            1_700_000_001,
            2,             // Short
            150_000_000,   // $150 entry, 6dp
            200_000_000,   // $200 notional, 6dp
            4_000_000,     // $4 collateral, 6dp
            -100_000,      // -$0.10 realised
            1_500_000_000, // 1.5 tokens locked
        );
        let address = Pubkey::new_unique();
        let pos = decode_position(address, &bytes).expect("decode");
        assert_eq!(pos.address, address);
        assert_eq!(pos.owner, owner);
        assert_eq!(pos.pool, pool);
        assert_eq!(pos.custody, custody);
        assert_eq!(pos.collateral_custody, coll);
        assert_eq!(pos.open_time, 1_700_000_000);
        assert_eq!(pos.update_time, 1_700_000_001);
        assert_eq!(pos.side, PerpSide::Short);
        assert_eq!(pos.price, 150_000_000);
        assert_eq!(pos.size_usd, 200_000_000);
        assert_eq!(pos.collateral_usd, 4_000_000);
        assert_eq!(pos.realised_pnl_usd, -100_000);
        assert_eq!(pos.locked_amount, 1_500_000_000);
        assert!(!pos.is_empty());
    }

    #[test]
    fn decode_position_rejects_short_slice() {
        let buf = vec![0u8; POSITION_MIN_LEN - 1];
        assert!(decode_position(Pubkey::default(), &buf).is_err());
    }

    #[test]
    fn decode_position_rejects_wrong_discriminator() {
        let mut buf = vec![0u8; POSITION_TOTAL_LEN];
        // Bad disc — anything other than POSITION_DISCRIMINATOR.
        buf[0] = 0xff;
        assert!(decode_position(Pubkey::default(), &buf).is_err());
    }

    #[test]
    fn decode_position_long_side_byte() {
        let bytes = build_position_bytes(
            Pubkey::default(),
            Pubkey::default(),
            Pubkey::default(),
            Pubkey::default(),
            0,
            0,
            1, // Long
            0,
            0,
            0,
            0,
            0,
        );
        let pos = decode_position(Pubkey::default(), &bytes).expect("decode");
        assert_eq!(pos.side, PerpSide::Long);
        assert!(pos.is_empty(), "size_usd=0 must report empty");
    }

    #[test]
    fn decode_custody_max_leverage_reads_offset_176() {
        // Build a 200-byte buffer with the live mainnet value
        // 19_531 (= 19.531× max leverage, thousandths scale) at
        // offset 176. See docs/jupiter-perps-position-spec.md §4.1.
        let mut buf = vec![0u8; 200];
        buf[176..184].copy_from_slice(&19_531u64.to_le_bytes());
        assert_eq!(decode_custody_max_leverage_bps(&buf), Some(19_531));
    }

    #[test]
    fn decode_custody_max_leverage_short_slice() {
        let buf = vec![0u8; 100];
        assert_eq!(decode_custody_max_leverage_bps(&buf), None);
    }
}
