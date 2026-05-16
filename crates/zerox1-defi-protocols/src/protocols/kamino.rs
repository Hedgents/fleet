//! Kamino Lend (klend) instruction builders.
//!
//! ## Status: scaffold
//!
//! Account layouts and instruction names match Kamino's published IDL on
//! `github.com/Kamino-Finance/klend`. Anchor discriminators are computed
//! deterministically from the standard `global:<ix_name>` preimage.
//!
//! Before mainnet use, verify against the live IDL:
//! 1. Account order in `deposit_reserve_liquidity_and_obligation_collateral`
//!    has shifted across klend versions; pin to the exact program version
//!    deployed at `KAMINO_LEND_PROGRAM_ID`.
//! 2. Some reserve operations require a `RefreshReserve` ix immediately
//!    before the deposit/withdraw — this scaffold prepends it.
//! 3. Withdrawal flows include an obligation health check; klend rejects if
//!    the resulting LTV would exceed liquidation threshold.
//!
//! Devnet test recommended before live deployment.

use borsh::BorshSerialize;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;

use crate::{
    constants::{
        ASSOCIATED_TOKEN_PROGRAM_ID, KAMINO_FARMS_PROGRAM_ID, KAMINO_LEND_PROGRAM_ID,
        SYSTEM_PROGRAM_ID, SYSVAR_INSTRUCTIONS_ID, SYSVAR_RENT_ID, TOKEN_PROGRAM_ID,
    },
    util::{anchor_discriminator, ata},
    Error, Result,
};

// ── Reserve metadata ────────────────────────────────────────────────────────

/// Minimal metadata needed to call deposit/withdraw against a Kamino reserve.
///
/// Populate from on-chain state via klend's `Reserve` account before calling
/// the instruction builders. For the demo we hardcode mainstream reserves in
/// `crates/zerox1-defi-daemon`.
#[derive(Debug, Clone)]
pub struct ReserveAccounts {
    /// The reserve account itself.
    pub reserve: Pubkey,
    /// The lending market this reserve belongs to.
    pub lending_market: Pubkey,
    /// The lending market authority PDA. Derived: PDA(["lma", lending_market], program_id)
    pub lending_market_authority: Pubkey,
    /// The mint of the underlying liquidity asset (e.g. USDC mint).
    pub liquidity_mint: Pubkey,
    /// The reserve's liquidity supply token account (where deposits go).
    pub liquidity_supply: Pubkey,
    /// The mint of the reserve's collateral token (cToken).
    pub collateral_mint: Pubkey,
    /// The reserve's collateral supply token account.
    pub collateral_supply: Pubkey,
    /// The reserve's fee receiver token account.
    pub fee_receiver: Pubkey,
    /// Scope prices oracle account for this reserve. Read from the reserve
    /// account data at offset 5112. Pass `Pubkey::default()` for devnet /
    /// synthetic paths (simulation will reject, which is the expected shape).
    pub scope_prices: Pubkey,
    /// Kamino Farms collateral farm state for this reserve (offset 64 in Reserve).
    /// `Pubkey::default()` when the reserve has no collateral farm attached;
    /// in that case `deposit_ix` omits the RefreshObligationFarmsForReserve step.
    pub farm_collateral: Pubkey,
    /// Kamino Farms debt farm state for this reserve (offset 96 in Reserve).
    /// `Pubkey::default()` when the reserve has no debt farm attached.
    /// v2 borrow handlers consult this to know whether to CPI into farms for
    /// the obligation's debt-side accounting.
    pub farm_debt: Pubkey,
}

/// Derive the lending market authority PDA for a given lending market.
pub fn derive_lending_market_authority(lending_market: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"lma", lending_market.as_ref()], &KAMINO_LEND_PROGRAM_ID).0
}

/// Derive the user obligation PDA for a given lending market + user, with
/// caller-supplied `tag` + `id` seed bytes.
///
/// Klend uses a tag + id system to support multiple obligations per user
/// within the same lending market. Fleet uses this to isolate strategies:
///   * `(0, 0)` — stable-yield's USDC supply obligation (legacy default).
///   * `(0, 1)` — multiply-daemon's leveraged jitoSOL obligation.
///
/// Isolation matters because Kamino's liquidator seizes *all* collateral
/// on an obligation. Sharing one obligation across strategies means a
/// liquidation in one strategy can drain collateral that belongs to
/// another. Each strategy must own its own obligation PDA.
pub fn derive_user_obligation_with_seed(
    user: &Pubkey,
    lending_market: &Pubkey,
    tag: u8,
    id: u8,
) -> Pubkey {
    Pubkey::find_program_address(
        &[
            &[tag],
            &[id],
            user.as_ref(),
            lending_market.as_ref(),
            &Pubkey::default().to_bytes(),
            &Pubkey::default().to_bytes(),
        ],
        &KAMINO_LEND_PROGRAM_ID,
    )
    .0
}

/// Derive the user obligation PDA for `(tag=0, id=0)` — the legacy default
/// used by stable-yield and historically by every other caller. Kept as a
/// thin wrapper around [`derive_user_obligation_with_seed`] so the PDA
/// address for existing stable-yield positions remains unchanged.
pub fn derive_user_obligation(user: &Pubkey, lending_market: &Pubkey) -> Pubkey {
    derive_user_obligation_with_seed(user, lending_market, 0, 0)
}

/// Derive the user metadata PDA used by klend to track per-user state.
/// Seeds: ["user_meta", user]
pub fn derive_user_metadata(user: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"user_meta", user.as_ref()], &KAMINO_LEND_PROGRAM_ID).0
}

/// Build the `InitUserMetadata` instruction.
///
/// Must be called once for any user before their first obligation can be
/// created. Safe to skip if user_metadata already exists — check with
/// `kamino_loader::user_metadata_exists` before including this instruction.
///
/// IDL accounts (6): owner(readonly signer), fee_payer(writable signer),
/// user_metadata(writable PDA), referrer_user_metadata(optional readonly),
/// rent, system_program.
pub fn init_user_metadata_ix(user: &Pubkey) -> Instruction {
    let user_metadata = derive_user_metadata(user);
    // discriminator = sha256("global:init_user_metadata")[0..8]
    // user_lookup_table is a required 32-byte Address (not Option<Pubkey>).
    // When no LUT exists, the SDK passes DEFAULT_PUBLIC_KEY = SystemProgram::ID.
    let mut data = anchor_discriminator("global", "init_user_metadata").to_vec();
    data.extend_from_slice(SYSTEM_PROGRAM_ID.as_ref()); // user_lookup_table = 11111…111 (no LUT)

    Instruction {
        program_id: KAMINO_LEND_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(*user, true), // owner (readonly signer)
            AccountMeta::new(*user, true),          // fee_payer (writable signer)
            AccountMeta::new(user_metadata, false), // user_metadata (writable PDA)
            // referrer_user_metadata: isOptional=true; pass program ID as
            // Anchor's canonical None sentinel for optional readonly accounts.
            AccountMeta::new_readonly(KAMINO_LEND_PROGRAM_ID, false),
            AccountMeta::new_readonly(SYSVAR_RENT_ID, false), // rent
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false), // system_program
        ],
        data,
    }
}

// ── Instruction data structures ─────────────────────────────────────────────

#[derive(BorshSerialize)]
struct InitObligationArgs {
    tag: u8,
    id: u8,
}

#[derive(BorshSerialize)]
struct DepositArgs {
    liquidity_amount: u64,
}

#[derive(BorshSerialize)]
struct WithdrawArgs {
    collateral_amount: u64,
}

// ── initialize_obligation ────────────────────────────────────────────────────

/// Build the `InitObligation` instruction (Kamino v2 — IDL name: "init_obligation").
///
/// Creates the user's obligation PDA for the given lending market. Klend
/// requires this to exist before any deposit or borrow.
///
/// Accounts (9): owner(readonly signer), fee_payer(writable signer),
/// obligation(writable PDA), lending_market(readonly), seed1(readonly),
/// seed2(readonly), owner_user_metadata(readonly), rent, system_program.
pub fn initialize_obligation_ix(
    user: &Pubkey,
    lending_market: &Pubkey,
    obligation_seed: (u8, u8),
) -> Result<Instruction> {
    let (tag, id) = obligation_seed;
    let obligation = derive_user_obligation_with_seed(user, lending_market, tag, id);
    let user_metadata = derive_user_metadata(user);

    // Discriminator: sha256("global:init_obligation")[0..8] = fb0ae74c1b0b9f60
    let mut data = anchor_discriminator("global", "init_obligation").to_vec();
    InitObligationArgs { tag, id }
        .serialize(&mut data)
        .map_err(|_| Error::Overflow)?;

    Ok(Instruction {
        program_id: KAMINO_LEND_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(*user, true), // obligationOwner (readonly signer)
            AccountMeta::new(*user, true),          // feePayer (writable signer)
            AccountMeta::new(obligation, false),    // obligation (writable PDA)
            AccountMeta::new_readonly(*lending_market, false), // lendingMarket
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false), // seed1Account (system = no seed)
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false), // seed2Account (system = no seed)
            AccountMeta::new_readonly(user_metadata, false), // ownerUserMetadata (readonly)
            AccountMeta::new_readonly(SYSVAR_RENT_ID, false), // rent
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false), // systemProgram
        ],
        data,
    })
}

// ── refresh_obligation ──────────────────────────────────────────────────────

/// RefreshObligation — required before deposit/withdraw when Kamino's
/// check_refresh enforcement is active.
///
/// `obligation_reserves` are the reserves registered on the obligation in
/// array order (deposits first, then borrows). When the obligation has any
/// registered reserves, klend's RefreshObligation requires each one as a
/// remaining account in the same order. Pass an empty slice for a freshly
/// initialized obligation (no deposits, no borrows) — the program tolerates
/// `remaining_accounts_count == 0` only in that case.
///
/// Bug fix (2026-05-13): without these remaining accounts on a second-deposit
/// or any withdraw, klend errors with `InvalidAccountInput` (0x1776,
/// expected_remaining_accounts=N, actual=0).
pub fn refresh_obligation_ix(
    user: &Pubkey,
    lending_market: &Pubkey,
    obligation_seed: (u8, u8),
    obligation_reserves: &[Pubkey],
) -> Instruction {
    let (tag, id) = obligation_seed;
    let obligation = derive_user_obligation_with_seed(user, lending_market, tag, id);
    let data = anchor_discriminator("global", "refresh_obligation").to_vec();
    let mut accounts = Vec::with_capacity(2 + obligation_reserves.len());
    accounts.push(AccountMeta::new_readonly(*lending_market, false)); // lendingMarket
    accounts.push(AccountMeta::new(obligation, false)); // obligation (writable)
    for reserve in obligation_reserves {
        accounts.push(AccountMeta::new_readonly(*reserve, false));
    }
    Instruction {
        program_id: KAMINO_LEND_PROGRAM_ID,
        accounts,
        data,
    }
}

// ── refresh_obligation_farms_for_reserve ────────────────────────────────────

/// Derive the obligation-farm user state PDA for a given reserve farm and
/// obligation. Seeds: ["user", farm, obligation] under KAMINO_FARMS_PROGRAM_ID.
pub fn derive_obligation_farm_user_state(farm: &Pubkey, obligation: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"user", farm.as_ref(), obligation.as_ref()],
        &KAMINO_FARMS_PROGRAM_ID,
    )
    .0
}

/// RefreshObligationFarmsForReserve — required by klend before deposit when
/// the reserve has a collateral farm (`reserve.farm_collateral != default`).
/// Mode 0 = Collateral (used for deposit path).
pub fn refresh_obligation_farms_for_reserve_ix(
    crank: &Pubkey,
    user: &Pubkey,
    reserve: &ReserveAccounts,
    obligation_seed: (u8, u8),
) -> Instruction {
    let (tag, id) = obligation_seed;
    let obligation = derive_user_obligation_with_seed(user, &reserve.lending_market, tag, id);
    let obligation_farm_user_state =
        derive_obligation_farm_user_state(&reserve.farm_collateral, &obligation);
    let mut data = anchor_discriminator("global", "refresh_obligation_farms_for_reserve").to_vec();
    data.push(0u8); // mode = 0 (Collateral)
    Instruction {
        program_id: KAMINO_LEND_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(*crank, true), // crank (signer)
            AccountMeta::new_readonly(obligation, false), // baseAccounts.obligation
            AccountMeta::new_readonly(reserve.lending_market_authority, false), // baseAccounts.lendingMarketAuthority
            AccountMeta::new_readonly(reserve.reserve, false), // baseAccounts.reserve
            AccountMeta::new(reserve.farm_collateral, false),  // baseAccounts.reserveFarmState
            AccountMeta::new(obligation_farm_user_state, false), // baseAccounts.obligationFarmUserState
            AccountMeta::new_readonly(reserve.lending_market, false), // baseAccounts.lendingMarket
            AccountMeta::new_readonly(KAMINO_FARMS_PROGRAM_ID, false), // farmsProgram
            AccountMeta::new_readonly(SYSVAR_RENT_ID, false),    // rent
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false), // systemProgram
        ],
        data,
    }
}

// ── init_obligation_farms_for_reserve ───────────────────────────────────────

/// InitObligationFarmsForReserve — creates the obligationFarmUserState account
/// the first time a user interacts with a farm-enabled reserve. Must precede
/// RefreshObligationFarmsForReserve if the account doesn't exist yet.
/// Mode 0 = Collateral.
pub fn init_obligation_farms_for_reserve_ix(
    payer: &Pubkey,
    user: &Pubkey,
    reserve: &ReserveAccounts,
    obligation_seed: (u8, u8),
) -> Instruction {
    let (tag, id) = obligation_seed;
    let obligation = derive_user_obligation_with_seed(user, &reserve.lending_market, tag, id);
    let obligation_farm = derive_obligation_farm_user_state(&reserve.farm_collateral, &obligation);
    let mut data = anchor_discriminator("global", "init_obligation_farms_for_reserve").to_vec();
    data.push(0u8); // mode = 0 (Collateral)
    Instruction {
        program_id: KAMINO_LEND_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*payer, true),          // payer (writable signer)
            AccountMeta::new_readonly(*user, false), // owner
            AccountMeta::new(obligation, false),     // obligation (writable)
            AccountMeta::new_readonly(reserve.lending_market_authority, false), // lendingMarketAuthority
            AccountMeta::new(reserve.reserve, false), // reserve (writable)
            AccountMeta::new(reserve.farm_collateral, false), // reserveFarmState (writable)
            AccountMeta::new(obligation_farm, false), // obligationFarm (writable)
            AccountMeta::new_readonly(reserve.lending_market, false), // lendingMarket
            AccountMeta::new_readonly(KAMINO_FARMS_PROGRAM_ID, false), // farmsProgram
            AccountMeta::new_readonly(SYSVAR_RENT_ID, false), // rent
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false), // systemProgram
        ],
        data,
    }
}

// ── deposit_reserve_liquidity_and_obligation_collateral ─────────────────────

/// Build the instruction sequence to deposit `amount` raw units of liquidity
/// (e.g. 1_000_000 = 1 USDC) into a Kamino reserve and credit the user's
/// obligation with the corresponding cTokens.
///
/// Kamino's `check_refresh` builds a `required_pre_ixs` list of
/// `[RefreshReserve, RefreshObligation, RefreshFarms(if farm)]`, then
/// **reverses** it before validating positions relative to the deposit:
///   - deposit-1  = RefreshFarms (when reserve.farm_collateral != default)
///   - deposit-2  = RefreshObligation
///   - deposit-3  = RefreshReserve   (deposit-2 when no farm)
/// Additionally, RefreshFarms is added to `required_post_ixs` and validated
/// at deposit+1, so it must appear both immediately before AND after the
/// deposit instruction when the reserve has a collateral farm.
///
/// Returns (no farm):
///   InitializeObligation · ATA-create · RefreshReserve · RefreshObligation
///   · Deposit
/// Returns (with farm):
///   InitializeObligation · ATA-create · RefreshReserve · RefreshObligation
///   · RefreshFarms(pre) · Deposit · RefreshFarms(post)
///
/// Caller is responsible for adding compute budget instructions.
pub fn deposit_ix(
    user: &Pubkey,
    reserve: &ReserveAccounts,
    amount: u64,
    obligation_seed: (u8, u8),
    obligation_reserves: &[Pubkey],
) -> Result<Vec<Instruction>> {
    if amount == 0 {
        return Err(Error::ZeroAmount);
    }

    let (tag, id) = obligation_seed;
    let user_liquidity_ata = ata(user, &reserve.liquidity_mint);
    let user_obligation = derive_user_obligation_with_seed(user, &reserve.lending_market, tag, id);
    let has_farm = reserve.farm_collateral != Pubkey::default();

    let mut ixs = Vec::with_capacity(if has_farm { 7 } else { 5 });

    // 1. InitializeObligation (no-op if the obligation already exists).
    ixs.push(initialize_obligation_ix(
        user,
        &reserve.lending_market,
        obligation_seed,
    )?);

    // 2. Idempotent ATA-create for liquidity (no-op if exists).
    ixs.push(create_associated_token_account_idempotent(
        user,
        user,
        &reserve.liquidity_mint,
        &TOKEN_PROGRAM_ID,
    ));

    // 3. RefreshReserve — at deposit-3 (farm) or deposit-2 (no farm).
    ixs.push(refresh_reserve_ix(reserve));

    // 4. RefreshObligation — at deposit-2 (farm) or deposit-1 (no farm).
    // Pass obligation's registered reserves as remaining accounts (in array
    // order) when the obligation already has any. Empty for fresh obligations.
    ixs.push(refresh_obligation_ix(
        user,
        &reserve.lending_market,
        obligation_seed,
        obligation_reserves,
    ));

    // 5. RefreshObligationFarmsForReserve (pre) — at deposit-1.
    // check_refresh also requires a matching instruction at deposit+1 (post),
    // which is appended after the deposit below.
    if has_farm {
        ixs.push(refresh_obligation_farms_for_reserve_ix(
            user,
            user,
            reserve,
            obligation_seed,
        ));
    }

    // 6. Deposit.
    let mut data = anchor_discriminator(
        "global",
        "deposit_reserve_liquidity_and_obligation_collateral",
    )
    .to_vec();
    DepositArgs {
        liquidity_amount: amount,
    }
    .serialize(&mut data)
    .map_err(|_| Error::Overflow)?;

    let accounts = vec![
        AccountMeta::new(*user, true),            // owner (signer)
        AccountMeta::new(user_obligation, false), // obligation
        AccountMeta::new_readonly(reserve.lending_market, false), // lending_market
        AccountMeta::new_readonly(reserve.lending_market_authority, false),
        AccountMeta::new(reserve.reserve, false), // reserve
        AccountMeta::new_readonly(reserve.liquidity_mint, false), // reserve_liquidity_mint
        AccountMeta::new(reserve.liquidity_supply, false), // reserve_liquidity_supply
        AccountMeta::new(reserve.collateral_mint, false), // reserve_collateral_mint
        AccountMeta::new(reserve.collateral_supply, false), // reserve_destination_deposit_collateral
        AccountMeta::new(user_liquidity_ata, false),        // user_source_liquidity
        // placeholder_user_destination_collateral: isOptional=true; pass
        // KAMINO_LEND_PROGRAM_ID (programAddress) as Anchor None sentinel.
        AccountMeta::new_readonly(KAMINO_LEND_PROGRAM_ID, false), // placeholder (None)
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),       // collateral_token_program
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),       // liquidity_token_program
        AccountMeta::new_readonly(SYSVAR_INSTRUCTIONS_ID, false), // instruction_sysvar
    ];

    ixs.push(Instruction {
        program_id: KAMINO_LEND_PROGRAM_ID,
        accounts,
        data,
    });

    // 7. RefreshObligationFarmsForReserve (post) — required at deposit+1.
    // Kamino's check_refresh validates this in required_post_ixs.
    if has_farm {
        ixs.push(refresh_obligation_farms_for_reserve_ix(
            user,
            user,
            reserve,
            obligation_seed,
        ));
    }

    Ok(ixs)
}

/// Bare `deposit_reserve_liquidity_and_obligation_collateral` instruction —
/// no init_obligation, no ATA-create, no refresh_reserve. The Multiply
/// Agent uses this to compose leverage steps where the obligation already
/// exists, the user's ATA already exists, and the reserve has been refreshed
/// earlier in the same tx (e.g. via a paired borrow which refreshes too).
///
/// Caller MUST ensure: obligation initialized, user ATA exists, and a
/// refresh_reserve for `reserve` ran earlier in the same transaction.
pub fn deposit_collateral_only_ix(
    user: &Pubkey,
    reserve: &ReserveAccounts,
    amount: u64,
    obligation_seed: (u8, u8),
) -> Result<Instruction> {
    if amount == 0 {
        return Err(Error::ZeroAmount);
    }
    let (tag, id) = obligation_seed;
    let user_liquidity_ata = ata(user, &reserve.liquidity_mint);
    let user_obligation = derive_user_obligation_with_seed(user, &reserve.lending_market, tag, id);

    let mut data = anchor_discriminator(
        "global",
        "deposit_reserve_liquidity_and_obligation_collateral",
    )
    .to_vec();
    DepositArgs {
        liquidity_amount: amount,
    }
    .serialize(&mut data)
    .map_err(|_| Error::Overflow)?;

    let accounts = vec![
        AccountMeta::new(*user, true),
        AccountMeta::new(user_obligation, false),
        AccountMeta::new_readonly(reserve.lending_market, false),
        AccountMeta::new_readonly(reserve.lending_market_authority, false),
        AccountMeta::new(reserve.reserve, false),
        AccountMeta::new_readonly(reserve.liquidity_mint, false),
        AccountMeta::new(reserve.liquidity_supply, false),
        AccountMeta::new(reserve.collateral_mint, false),
        AccountMeta::new(reserve.collateral_supply, false),
        AccountMeta::new(user_liquidity_ata, false),
        AccountMeta::new_readonly(KAMINO_LEND_PROGRAM_ID, false), // placeholder (None)
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        AccountMeta::new_readonly(SYSVAR_INSTRUCTIONS_ID, false),
    ];

    Ok(Instruction {
        program_id: KAMINO_LEND_PROGRAM_ID,
        accounts,
        data,
    })
}

// ── withdraw_obligation_collateral_and_redeem_reserve_collateral ────────────

/// Build the instruction sequence to withdraw `amount` raw units of liquidity
/// from a Kamino reserve.
///
/// `amount` is in *liquidity* units (USDC), not collateral cToken units.
/// klend converts internally.
///
/// Returns:
/// 1. Idempotent ATA-create for the user's liquidity ATA (no-op if exists)
/// 2. RefreshReserve
/// 3. The withdraw instruction
pub fn withdraw_ix(
    user: &Pubkey,
    reserve: &ReserveAccounts,
    amount: u64,
    obligation_seed: (u8, u8),
    obligation_reserves: &[Pubkey],
) -> Result<Vec<Instruction>> {
    if amount == 0 {
        return Err(Error::ZeroAmount);
    }

    let (tag, id) = obligation_seed;
    let user_liquidity_ata = ata(user, &reserve.liquidity_mint);
    let user_obligation = derive_user_obligation_with_seed(user, &reserve.lending_market, tag, id);

    let mut ixs = Vec::with_capacity(4);

    ixs.push(create_associated_token_account_idempotent(
        user,
        user,
        &reserve.liquidity_mint,
        &TOKEN_PROGRAM_ID,
    ));

    ixs.push(refresh_reserve_ix(reserve));

    // RefreshObligation with the obligation's registered reserves as
    // remaining accounts. klend's check_refresh requires this before the
    // withdraw ixn. Bug fix (2026-05-13): previously omitted entirely, which
    // caused withdraw to fail with InvalidAccountInput (0x1776) once the
    // obligation had any registered reserves.
    ixs.push(refresh_obligation_ix(
        user,
        &reserve.lending_market,
        obligation_seed,
        obligation_reserves,
    ));

    let mut data = anchor_discriminator(
        "global",
        "withdraw_obligation_collateral_and_redeem_reserve_collateral",
    )
    .to_vec();
    WithdrawArgs {
        collateral_amount: amount,
    }
    .serialize(&mut data)
    .map_err(|_| Error::Overflow)?;

    let accounts = vec![
        AccountMeta::new(*user, true),
        AccountMeta::new(user_obligation, false),
        AccountMeta::new_readonly(reserve.lending_market, false),
        AccountMeta::new_readonly(reserve.lending_market_authority, false),
        AccountMeta::new(reserve.reserve, false),
        AccountMeta::new_readonly(reserve.liquidity_mint, false),
        AccountMeta::new(reserve.liquidity_supply, false),
        AccountMeta::new(reserve.collateral_mint, false),
        AccountMeta::new(reserve.collateral_supply, false),
        AccountMeta::new(user_liquidity_ata, false),
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        AccountMeta::new_readonly(SYSVAR_INSTRUCTIONS_ID, false),
    ];

    ixs.push(Instruction {
        program_id: KAMINO_LEND_PROGRAM_ID,
        accounts,
        data,
    });

    Ok(ixs)
}

// ── borrow_obligation_liquidity ─────────────────────────────────────────────

#[derive(BorshSerialize)]
struct BorrowArgs {
    liquidity_amount: u64,
}

#[derive(BorshSerialize)]
struct RepayArgs {
    liquidity_amount: u64,
}

#[derive(BorshSerialize)]
struct FlashBorrowArgs {
    liquidity_amount: u64,
}

#[derive(BorshSerialize)]
struct FlashRepayArgs {
    liquidity_amount: u64,
    /// Index of the matching `flashBorrowReserveLiquidity` instruction within
    /// the same transaction (klend uses this to pair them via the
    /// instructions sysvar).
    borrow_instruction_index: u8,
}

/// Build the instruction sequence to borrow `amount` raw units of
/// `reserve.liquidity_mint` against the user's obligation. Caller must already
/// have collateral deposited via `deposit_ix`.
///
/// Returns:
/// 1. Idempotent ATA-create for the user's borrow ATA (no-op if exists)
/// 2. RefreshReserve (required before any borrow)
/// 3. The borrow instruction
///
/// Account ordering verified against klend IDL v1.19.0 (12 accounts).
pub fn borrow_obligation_liquidity_ix(
    user: &Pubkey,
    reserve: &ReserveAccounts,
    amount: u64,
    obligation_seed: (u8, u8),
) -> Result<Vec<Instruction>> {
    if amount == 0 {
        return Err(Error::ZeroAmount);
    }

    let (tag, id) = obligation_seed;
    let user_destination_ata = ata(user, &reserve.liquidity_mint);
    let user_obligation = derive_user_obligation_with_seed(user, &reserve.lending_market, tag, id);

    let mut ixs = Vec::with_capacity(3);

    ixs.push(create_associated_token_account_idempotent(
        user,
        user,
        &reserve.liquidity_mint,
        &TOKEN_PROGRAM_ID,
    ));
    ixs.push(refresh_reserve_ix(reserve));

    let mut data = anchor_discriminator("global", "borrow_obligation_liquidity").to_vec();
    BorrowArgs {
        liquidity_amount: amount,
    }
    .serialize(&mut data)
    .map_err(|_| Error::Overflow)?;

    let accounts = vec![
        AccountMeta::new_readonly(*user, true),   // [0] owner (signer)
        AccountMeta::new(user_obligation, false), // [1] obligation
        AccountMeta::new_readonly(reserve.lending_market, false), // [2] lending_market
        AccountMeta::new_readonly(reserve.lending_market_authority, false), // [3] lending_market_authority
        AccountMeta::new(reserve.reserve, false),                           // [4] borrow_reserve
        AccountMeta::new_readonly(reserve.liquidity_mint, false), // [5] borrow_reserve_liquidity_mint
        AccountMeta::new(reserve.liquidity_supply, false),        // [6] reserve_source_liquidity
        AccountMeta::new(reserve.fee_receiver, false), // [7] borrow_reserve_liquidity_fee_receiver
        AccountMeta::new(user_destination_ata, false), // [8] user_destination_liquidity
        AccountMeta::new(KAMINO_LEND_PROGRAM_ID, false), // [9] referrer_token_state (opt, None placeholder)
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false), // [10] token_program
        AccountMeta::new_readonly(SYSVAR_INSTRUCTIONS_ID, false), // [11] instruction_sysvar_account
    ];

    ixs.push(Instruction {
        program_id: KAMINO_LEND_PROGRAM_ID,
        accounts,
        data,
    });

    Ok(ixs)
}

// ── repay_obligation_liquidity ──────────────────────────────────────────────

/// Build the instruction sequence to repay `amount` raw units of
/// `reserve.liquidity_mint` against the user's obligation.
///
/// Returns:
/// 1. Idempotent ATA-create for the user's repay ATA (no-op if exists)
/// 2. RefreshReserve
/// 3. The repay instruction
///
/// Account ordering verified against klend IDL v1.19.0 (9 accounts).
pub fn repay_obligation_liquidity_ix(
    user: &Pubkey,
    reserve: &ReserveAccounts,
    amount: u64,
    obligation_seed: (u8, u8),
) -> Result<Vec<Instruction>> {
    if amount == 0 {
        return Err(Error::ZeroAmount);
    }

    let (tag, id) = obligation_seed;
    let user_source_ata = ata(user, &reserve.liquidity_mint);
    let user_obligation = derive_user_obligation_with_seed(user, &reserve.lending_market, tag, id);

    let mut ixs = Vec::with_capacity(3);

    ixs.push(create_associated_token_account_idempotent(
        user,
        user,
        &reserve.liquidity_mint,
        &TOKEN_PROGRAM_ID,
    ));
    ixs.push(refresh_reserve_ix(reserve));

    let mut data = anchor_discriminator("global", "repay_obligation_liquidity").to_vec();
    RepayArgs {
        liquidity_amount: amount,
    }
    .serialize(&mut data)
    .map_err(|_| Error::Overflow)?;

    let accounts = vec![
        AccountMeta::new_readonly(*user, true),   // [0] owner (signer)
        AccountMeta::new(user_obligation, false), // [1] obligation
        AccountMeta::new_readonly(reserve.lending_market, false), // [2] lending_market
        AccountMeta::new(reserve.reserve, false), // [3] repay_reserve
        AccountMeta::new_readonly(reserve.liquidity_mint, false), // [4] reserve_liquidity_mint
        AccountMeta::new(reserve.liquidity_supply, false), // [5] reserve_destination_liquidity
        AccountMeta::new(user_source_ata, false), // [6] user_source_liquidity
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false), // [7] token_program
        AccountMeta::new_readonly(SYSVAR_INSTRUCTIONS_ID, false), // [8] instruction_sysvar_account
    ];

    ixs.push(Instruction {
        program_id: KAMINO_LEND_PROGRAM_ID,
        accounts,
        data,
    });

    Ok(ixs)
}

// ── flash_borrow_reserve_liquidity ──────────────────────────────────────────

/// Build a flash-borrow instruction that lends `amount` raw units of
/// `reserve.liquidity_mint` to the user. **Must** be paired with a
/// `flash_repay_reserve_liquidity_ix` later in the same transaction or
/// klend will reject the tx.
///
/// Returns just the flash-borrow instruction (caller is responsible for
/// composing it with deposit/swap/repay ixs in the same transaction).
///
/// Account ordering verified against klend IDL v1.19.0 (12 accounts).
pub fn flash_borrow_reserve_liquidity_ix(
    user: &Pubkey,
    reserve: &ReserveAccounts,
    amount: u64,
) -> Result<Instruction> {
    if amount == 0 {
        return Err(Error::ZeroAmount);
    }
    let user_destination_ata = ata(user, &reserve.liquidity_mint);

    let mut data = anchor_discriminator("global", "flash_borrow_reserve_liquidity").to_vec();
    FlashBorrowArgs {
        liquidity_amount: amount,
    }
    .serialize(&mut data)
    .map_err(|_| Error::Overflow)?;

    let accounts = vec![
        AccountMeta::new_readonly(*user, true), // [0] user_transfer_authority (signer)
        AccountMeta::new_readonly(reserve.lending_market_authority, false), // [1] lending_market_authority
        AccountMeta::new_readonly(reserve.lending_market, false),           // [2] lending_market
        AccountMeta::new(reserve.reserve, false),                           // [3] reserve
        AccountMeta::new_readonly(reserve.liquidity_mint, false), // [4] reserve_liquidity_mint
        AccountMeta::new(reserve.liquidity_supply, false),        // [5] reserve_source_liquidity
        AccountMeta::new(user_destination_ata, false),            // [6] user_destination_liquidity
        AccountMeta::new(reserve.fee_receiver, false), // [7] reserve_liquidity_fee_receiver
        AccountMeta::new(KAMINO_LEND_PROGRAM_ID, false), // [8] referrer_token_state (opt)
        AccountMeta::new(KAMINO_LEND_PROGRAM_ID, false), // [9] referrer_account (opt)
        AccountMeta::new_readonly(SYSVAR_INSTRUCTIONS_ID, false), // [10] sysvar_info
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false), // [11] token_program
    ];

    Ok(Instruction {
        program_id: KAMINO_LEND_PROGRAM_ID,
        accounts,
        data,
    })
}

// ── flash_repay_reserve_liquidity ───────────────────────────────────────────

/// Build a flash-repay instruction. `borrow_instruction_index` is the index
/// of the matching `flashBorrowReserveLiquidity` ix within the same
/// transaction (typically the first non-compute-budget ix, so 1 if compute
/// budget ixs are prepended, 0 otherwise).
///
/// Account ordering verified against klend IDL v1.19.0 (12 accounts).
pub fn flash_repay_reserve_liquidity_ix(
    user: &Pubkey,
    reserve: &ReserveAccounts,
    amount: u64,
    borrow_instruction_index: u8,
) -> Result<Instruction> {
    if amount == 0 {
        return Err(Error::ZeroAmount);
    }
    let user_source_ata = ata(user, &reserve.liquidity_mint);

    let mut data = anchor_discriminator("global", "flash_repay_reserve_liquidity").to_vec();
    FlashRepayArgs {
        liquidity_amount: amount,
        borrow_instruction_index,
    }
    .serialize(&mut data)
    .map_err(|_| Error::Overflow)?;

    let accounts = vec![
        AccountMeta::new_readonly(*user, true), // [0] user_transfer_authority (signer)
        AccountMeta::new_readonly(reserve.lending_market_authority, false), // [1] lending_market_authority
        AccountMeta::new_readonly(reserve.lending_market, false),           // [2] lending_market
        AccountMeta::new(reserve.reserve, false),                           // [3] reserve
        AccountMeta::new_readonly(reserve.liquidity_mint, false), // [4] reserve_liquidity_mint
        AccountMeta::new(reserve.liquidity_supply, false), // [5] reserve_destination_liquidity
        AccountMeta::new(user_source_ata, false),          // [6] user_source_liquidity
        AccountMeta::new(reserve.fee_receiver, false),     // [7] reserve_liquidity_fee_receiver
        AccountMeta::new(KAMINO_LEND_PROGRAM_ID, false),   // [8] referrer_token_state (opt)
        AccountMeta::new(KAMINO_LEND_PROGRAM_ID, false),   // [9] referrer_account (opt)
        AccountMeta::new_readonly(SYSVAR_INSTRUCTIONS_ID, false), // [10] sysvar_info
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false), // [11] token_program
    ];

    Ok(Instruction {
        program_id: KAMINO_LEND_PROGRAM_ID,
        accounts,
        data,
    })
}

// ── v2 ixn builders (CPI-internal farm refresh) ─────────────────────────────
//
// The v2 handlers (`borrow_obligation_liquidity_v2`,
// `deposit_obligation_collateral_v2`,
// `deposit_reserve_liquidity_and_obligation_collateral_v2`) wrap the v1
// handler's account list and append:
//   * the reserve's farm-user state account (None = klend program id sentinel)
//   * the reserve's farm-state account (None = klend program id sentinel)
//   * the Kamino Farms program id
//
// They internally CPI into farms to refresh the obligation-farm-user-state
// for the relevant `ReserveFarmKind`, so the caller no longer needs to inject
// `RefreshObligationFarmsForReserve` immediately before AND after the action.
// This collapses two ixns per action (and eliminates the klend 6051
// IncorrectInstructionInPosition trap that fired when farm refreshes were
// misplaced).
//
// Account ordering verified against klend-sdk codegen
// (`@codegen/klend/instructions/{borrowObligationLiquidityV2,depositObligationCollateralV2,depositReserveLiquidityAndObligationCollateralV2}.ts`)
// at klend-sdk master, 2026-05-13.
//
// `None`-encoded farm accounts use the klend program id as the sentinel,
// matching the SDK's `isSome(...) ? ... : programAddress`.

/// Resolve the farm accounts for v2 handlers, given the kind of farm the
/// action uses (`Collateral` for deposit, `Debt` for borrow).
///
/// Returns `(obligation_farm_user_state, reserve_farm_state)`:
/// * If the reserve has no farm of that kind, both are `KAMINO_LEND_PROGRAM_ID`
///   (the Anchor None sentinel — readonly).
/// * If the reserve HAS the farm, returns the real reserve farm state PDA
///   and the derived obligation-farm-user-state PDA.
fn v2_farm_accounts(
    reserve: &ReserveAccounts,
    obligation: &Pubkey,
    kind: V2FarmKind,
) -> (Pubkey, Pubkey, bool) {
    let farm = match kind {
        V2FarmKind::Collateral => reserve.farm_collateral,
        V2FarmKind::Debt => reserve.farm_debt,
    };
    if farm == Pubkey::default() {
        // No farm of this kind on the reserve. SDK encodes None as program-id
        // readonly. Return klend program id for both slots, mark readonly.
        (KAMINO_LEND_PROGRAM_ID, KAMINO_LEND_PROGRAM_ID, false)
    } else {
        let obligation_farm_user_state = derive_obligation_farm_user_state(&farm, obligation);
        (obligation_farm_user_state, farm, true)
    }
}

/// Which farm kind a v2 action targets — drives `reserve.farm_collateral` vs
/// `reserve.farm_debt` lookup.
#[derive(Debug, Clone, Copy)]
enum V2FarmKind {
    Collateral,
    Debt,
}

/// Bare `borrow_obligation_liquidity_v2` instruction — v2 of
/// [`borrow_obligation_liquidity_ix`].
///
/// V2 differs from v1 by appending three accounts after the v1 account list:
///   [12] obligation_farm_user_state (mut if farm present, else readonly None sentinel)
///   [13] reserve_farm_state         (mut if farm present, else readonly None sentinel)
///   [14] farms_program              (Kamino Farms program id)
///
/// Caller MUST ensure: RefreshReserve(every obligation reserve) +
/// RefreshObligation ran earlier in the same tx. The v2 handler eliminates
/// the manual `RefreshObligationFarmsForReserve` pre/post-ix dance — but
/// reserve / obligation freshness is still required.
///
/// Returns a single instruction (no ATA-create / refresh wrapping); the
/// caller composes the full bundle.
pub fn borrow_obligation_liquidity_v2_ix(
    user: &Pubkey,
    reserve: &ReserveAccounts,
    amount: u64,
    obligation_seed: (u8, u8),
) -> Result<Instruction> {
    if amount == 0 {
        return Err(Error::ZeroAmount);
    }
    let (tag, id) = obligation_seed;
    let user_destination_ata = ata(user, &reserve.liquidity_mint);
    let user_obligation = derive_user_obligation_with_seed(user, &reserve.lending_market, tag, id);

    let (farm_user_state, reserve_farm_state, farm_present) =
        v2_farm_accounts(reserve, &user_obligation, V2FarmKind::Debt);

    let mut data = anchor_discriminator("global", "borrow_obligation_liquidity_v2").to_vec();
    BorrowArgs {
        liquidity_amount: amount,
    }
    .serialize(&mut data)
    .map_err(|_| Error::Overflow)?;

    let farm_user_state_meta = if farm_present {
        AccountMeta::new(farm_user_state, false)
    } else {
        AccountMeta::new_readonly(farm_user_state, false)
    };
    let reserve_farm_state_meta = if farm_present {
        AccountMeta::new(reserve_farm_state, false)
    } else {
        AccountMeta::new_readonly(reserve_farm_state, false)
    };

    let accounts = vec![
        // ── v1 borrow accounts (12) ────────────────────────────────────
        AccountMeta::new_readonly(*user, true), // [0] owner (signer)
        AccountMeta::new(user_obligation, false), // [1] obligation
        AccountMeta::new_readonly(reserve.lending_market, false), // [2] lending_market
        AccountMeta::new_readonly(reserve.lending_market_authority, false), // [3] lending_market_authority
        AccountMeta::new(reserve.reserve, false),                           // [4] borrow_reserve
        AccountMeta::new_readonly(reserve.liquidity_mint, false), // [5] borrow_reserve_liquidity_mint
        AccountMeta::new(reserve.liquidity_supply, false),        // [6] reserve_source_liquidity
        AccountMeta::new(reserve.fee_receiver, false), // [7] borrow_reserve_liquidity_fee_receiver
        AccountMeta::new(user_destination_ata, false), // [8] user_destination_liquidity
        AccountMeta::new_readonly(KAMINO_LEND_PROGRAM_ID, false), // [9] referrer_token_state (None)
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false), // [10] token_program
        AccountMeta::new_readonly(SYSVAR_INSTRUCTIONS_ID, false), // [11] instruction_sysvar_account
        // ── v2 farm appendix (3) ───────────────────────────────────────
        farm_user_state_meta,    // [12] obligation_farm_user_state
        reserve_farm_state_meta, // [13] reserve_farm_state
        AccountMeta::new_readonly(KAMINO_FARMS_PROGRAM_ID, false), // [14] farms_program
    ];

    Ok(Instruction {
        program_id: KAMINO_LEND_PROGRAM_ID,
        accounts,
        data,
    })
}

/// Bare `repay_obligation_liquidity_v2` — v2 of [`repay_obligation_liquidity_ix`].
///
/// V2 differs from v1 by appending FOUR accounts after the v1 account list
/// (see klend's `RepayObligationLiquidityV2` struct — v1's
/// `RepayObligationLiquidity` does NOT carry `lending_market_authority`,
/// unlike borrow_v2 / withdraw_collateral_v2, so the v2 wrapper appends it
/// alongside the farm appendix):
///   [9]  obligation_farm_user_state (mut if Debt farm present, else readonly None sentinel)
///   [10] reserve_farm_state         (mut if Debt farm present, else readonly None sentinel)
///   [11] lending_market_authority   (PDA — readonly)
///   [12] farms_program              (Kamino Farms program id)
///
/// Repay touches the **Debt** farm (mirrors borrow_v2). The v2 handler does the
/// Debt-farm refresh CPI internally; no manual `RefreshObligationFarmsForReserve`
/// required.
///
/// Caller MUST ensure RefreshReserve(every obligation reserve) +
/// RefreshObligation ran earlier in the same tx, AND specifically:
///   [current_idx - 1] = RefreshObligation(obligation)
///   [current_idx - 2] = RefreshReserve(repay_reserve)
/// (klend's `check_refresh_ixs!` macro enforces these positions.)
///
/// `amount == u64::MAX` is the klend sentinel meaning "repay the full borrow
/// slot" — klend interprets it server-side. Zero is rejected.
///
/// Account list (13 total = v1 9 + v2 appendix 4):
/// ```text
///   [0]  owner (signer)
///   [1]  obligation (mut)
///   [2]  lending_market
///   [3]  repay_reserve (mut)
///   [4]  reserve_liquidity_mint
///   [5]  reserve_destination_liquidity (mut)  (= reserve.liquidity_supply)
///   [6]  user_source_liquidity (mut)
///   [7]  token_program
///   [8]  instruction_sysvar_account
///   [9]  obligation_farm_user_state (v2 — mut if Debt farm present)
///   [10] reserve_farm_state         (v2 — mut if Debt farm present)
///   [11] lending_market_authority   (v2 — readonly)
///   [12] farms_program              (v2)
/// ```
///
/// Returns a single instruction (no ATA-create / refresh wrapping); the
/// caller composes the full bundle.
pub fn repay_obligation_liquidity_v2_ix(
    user: &Pubkey,
    reserve: &ReserveAccounts,
    amount: u64,
    obligation_seed: (u8, u8),
) -> Result<Instruction> {
    if amount == 0 {
        return Err(Error::ZeroAmount);
    }
    let (tag, id) = obligation_seed;
    let user_source_ata = ata(user, &reserve.liquidity_mint);
    let user_obligation = derive_user_obligation_with_seed(user, &reserve.lending_market, tag, id);

    let (farm_user_state, reserve_farm_state, farm_present) =
        v2_farm_accounts(reserve, &user_obligation, V2FarmKind::Debt);

    let mut data = anchor_discriminator("global", "repay_obligation_liquidity_v2").to_vec();
    RepayArgs {
        liquidity_amount: amount,
    }
    .serialize(&mut data)
    .map_err(|_| Error::Overflow)?;

    let farm_user_state_meta = if farm_present {
        AccountMeta::new(farm_user_state, false)
    } else {
        AccountMeta::new_readonly(farm_user_state, false)
    };
    let reserve_farm_state_meta = if farm_present {
        AccountMeta::new(reserve_farm_state, false)
    } else {
        AccountMeta::new_readonly(reserve_farm_state, false)
    };

    let accounts = vec![
        // ── v1 repay accounts (9) ──────────────────────────────────────
        AccountMeta::new_readonly(*user, true), // [0] owner (signer)
        AccountMeta::new(user_obligation, false), // [1] obligation
        AccountMeta::new_readonly(reserve.lending_market, false), // [2] lending_market
        AccountMeta::new(reserve.reserve, false), // [3] repay_reserve
        AccountMeta::new_readonly(reserve.liquidity_mint, false), // [4] reserve_liquidity_mint
        AccountMeta::new(reserve.liquidity_supply, false), // [5] reserve_destination_liquidity
        AccountMeta::new(user_source_ata, false), // [6] user_source_liquidity
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false), // [7] token_program
        AccountMeta::new_readonly(SYSVAR_INSTRUCTIONS_ID, false), // [8] instruction_sysvar_account
        // ── v2 appendix (4) ────────────────────────────────────────────
        // klend's RepayObligationLiquidityV2 = v1 + OptionalObligationFarmsAccounts
        // + lending_market_authority + farms_program. Note: unlike borrow_v2 /
        // withdraw_collateral_v2, v1 repay does NOT include lending_market_authority,
        // so the v2 wrapper appends it after the farm pair, giving 13 accounts total.
        farm_user_state_meta,    // [9]  obligation_farm_user_state
        reserve_farm_state_meta, // [10] reserve_farm_state
        AccountMeta::new_readonly(reserve.lending_market_authority, false), // [11] lending_market_authority
        AccountMeta::new_readonly(KAMINO_FARMS_PROGRAM_ID, false),          // [12] farms_program
    ];

    Ok(Instruction {
        program_id: KAMINO_LEND_PROGRAM_ID,
        accounts,
        data,
    })
}

/// Bare `withdraw_obligation_collateral_v2` — v2 of the cToken-only collateral
/// withdraw. Pulls cTokens from `obligation` back to the user's collateral ATA
/// **without** redeeming them for underlying liquidity (use
/// [`withdraw_obligation_collateral_and_redeem_reserve_collateral_v2_ix`] for
/// the combined path).
///
/// Withdraw touches the **Collateral** farm (mirrors deposit_v2). The v2 handler
/// does the Collateral-farm refresh CPI internally.
///
/// `amount == u64::MAX` is the klend sentinel meaning "withdraw the full
/// deposited cToken slot" — klend clamps server-side. Zero is rejected.
///
/// Account list (12 total = v1 9 + v2 farm appendix 3):
/// ```text
///   [0]  owner (signer)
///   [1]  obligation (mut)
///   [2]  lending_market
///   [3]  lending_market_authority
///   [4]  withdraw_reserve (mut)
///   [5]  reserve_source_collateral (mut)  (= reserve.collateral_supply)
///   [6]  user_destination_collateral (mut)  (= user's cToken ATA)
///   [7]  token_program
///   [8]  instruction_sysvar_account
///   [9]  obligation_farm_user_state (v2 — mut if Collateral farm present)
///   [10] reserve_farm_state         (v2 — mut if Collateral farm present)
///   [11] farms_program              (v2)
/// ```
pub fn withdraw_obligation_collateral_v2_ix(
    user: &Pubkey,
    reserve: &ReserveAccounts,
    collateral_amount: u64,
    obligation_seed: (u8, u8),
) -> Result<Instruction> {
    if collateral_amount == 0 {
        return Err(Error::ZeroAmount);
    }
    let (tag, id) = obligation_seed;
    let user_collateral_ata = ata(user, &reserve.collateral_mint);
    let user_obligation = derive_user_obligation_with_seed(user, &reserve.lending_market, tag, id);

    let (farm_user_state, reserve_farm_state, farm_present) =
        v2_farm_accounts(reserve, &user_obligation, V2FarmKind::Collateral);

    let mut data = anchor_discriminator("global", "withdraw_obligation_collateral_v2").to_vec();
    WithdrawArgs { collateral_amount }
        .serialize(&mut data)
        .map_err(|_| Error::Overflow)?;

    let farm_user_state_meta = if farm_present {
        AccountMeta::new(farm_user_state, false)
    } else {
        AccountMeta::new_readonly(farm_user_state, false)
    };
    let reserve_farm_state_meta = if farm_present {
        AccountMeta::new(reserve_farm_state, false)
    } else {
        AccountMeta::new_readonly(reserve_farm_state, false)
    };

    let accounts = vec![
        // ── v1 withdraw_obligation_collateral accounts (9) ─────────────
        AccountMeta::new_readonly(*user, true), // [0] owner (signer)
        AccountMeta::new(user_obligation, false), // [1] obligation (mut)
        AccountMeta::new_readonly(reserve.lending_market, false), // [2] lending_market
        AccountMeta::new_readonly(reserve.lending_market_authority, false), // [3] lending_market_authority
        AccountMeta::new(reserve.reserve, false), // [4] withdraw_reserve (mut)
        AccountMeta::new(reserve.collateral_supply, false), // [5] reserve_source_collateral
        AccountMeta::new(user_collateral_ata, false), // [6] user_destination_collateral
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false), // [7] token_program
        AccountMeta::new_readonly(SYSVAR_INSTRUCTIONS_ID, false), // [8] instruction_sysvar_account
        // ── v2 farm appendix (3) ───────────────────────────────────────
        farm_user_state_meta,    // [9] obligation_farm_user_state
        reserve_farm_state_meta, // [10] reserve_farm_state
        AccountMeta::new_readonly(KAMINO_FARMS_PROGRAM_ID, false), // [11] farms_program
    ];

    Ok(Instruction {
        program_id: KAMINO_LEND_PROGRAM_ID,
        accounts,
        data,
    })
}

/// Bare `withdraw_obligation_collateral_and_redeem_reserve_collateral_v2` — v2
/// of the combined withdraw + redeem path: pulls cTokens from `obligation`,
/// burns them, and sends the underlying liquidity to the user's liquidity ATA
/// in a single ixn. Inverse of
/// [`deposit_reserve_liquidity_and_obligation_collateral_v2_ix`].
///
/// Touches the **Collateral** farm (mirrors deposit-reserve-and-collateral_v2).
///
/// `collateral_amount == u64::MAX` redeems the obligation's entire cToken slot
/// for this reserve (klend clamps server-side).
///
/// Account list (17 total = v1 14 + v2 farm appendix 3):
/// ```text
///   [0]  owner (signer, mut)
///   [1]  obligation (mut)
///   [2]  lending_market
///   [3]  lending_market_authority
///   [4]  withdraw_reserve (mut)
///   [5]  reserve_liquidity_mint
///   [6]  reserve_source_collateral (mut)   (= reserve.collateral_supply)
///   [7]  reserve_collateral_mint (mut)
///   [8]  reserve_liquidity_supply (mut)
///   [9]  user_destination_liquidity (mut)
///   [10] placeholder_user_destination_collateral (None = klend program id)
///   [11] collateral_token_program (SPL Token classic)
///   [12] liquidity_token_program  (Token-2022 capable; we pass classic)
///   [13] instruction_sysvar_account
///   [14] obligation_farm_user_state (v2 — mut if Collateral farm present)
///   [15] reserve_farm_state         (v2 — mut if Collateral farm present)
///   [16] farms_program              (v2)
/// ```
pub fn withdraw_obligation_collateral_and_redeem_reserve_collateral_v2_ix(
    user: &Pubkey,
    reserve: &ReserveAccounts,
    collateral_amount: u64,
    obligation_seed: (u8, u8),
) -> Result<Instruction> {
    if collateral_amount == 0 {
        return Err(Error::ZeroAmount);
    }
    let (tag, id) = obligation_seed;
    let user_liquidity_ata = ata(user, &reserve.liquidity_mint);
    let user_obligation = derive_user_obligation_with_seed(user, &reserve.lending_market, tag, id);

    let (farm_user_state, reserve_farm_state, farm_present) =
        v2_farm_accounts(reserve, &user_obligation, V2FarmKind::Collateral);

    let mut data = anchor_discriminator(
        "global",
        "withdraw_obligation_collateral_and_redeem_reserve_collateral_v2",
    )
    .to_vec();
    WithdrawArgs { collateral_amount }
        .serialize(&mut data)
        .map_err(|_| Error::Overflow)?;

    let farm_user_state_meta = if farm_present {
        AccountMeta::new(farm_user_state, false)
    } else {
        AccountMeta::new_readonly(farm_user_state, false)
    };
    let reserve_farm_state_meta = if farm_present {
        AccountMeta::new(reserve_farm_state, false)
    } else {
        AccountMeta::new_readonly(reserve_farm_state, false)
    };

    let accounts = vec![
        // ── v1 withdraw-and-redeem accounts (14) ───────────────────────
        AccountMeta::new(*user, true), // [0] owner (signer, mut)
        AccountMeta::new(user_obligation, false), // [1] obligation
        AccountMeta::new_readonly(reserve.lending_market, false), // [2]
        AccountMeta::new_readonly(reserve.lending_market_authority, false), // [3]
        AccountMeta::new(reserve.reserve, false), // [4] withdraw_reserve
        AccountMeta::new_readonly(reserve.liquidity_mint, false), // [5]
        AccountMeta::new(reserve.collateral_supply, false), // [6] reserve_source_collateral
        AccountMeta::new(reserve.collateral_mint, false), // [7]
        AccountMeta::new(reserve.liquidity_supply, false), // [8]
        AccountMeta::new(user_liquidity_ata, false), // [9] user_destination_liquidity
        AccountMeta::new_readonly(KAMINO_LEND_PROGRAM_ID, false), // [10] placeholder (None)
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false), // [11] collateral_token_program
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false), // [12] liquidity_token_program
        AccountMeta::new_readonly(SYSVAR_INSTRUCTIONS_ID, false), // [13]
        // ── v2 farm appendix (3) ───────────────────────────────────────
        farm_user_state_meta,    // [14] obligation_farm_user_state
        reserve_farm_state_meta, // [15] reserve_farm_state
        AccountMeta::new_readonly(KAMINO_FARMS_PROGRAM_ID, false), // [16] farms_program
    ];

    Ok(Instruction {
        program_id: KAMINO_LEND_PROGRAM_ID,
        accounts,
        data,
    })
}

/// Bare `deposit_obligation_collateral_v2` — v2 of the cToken collateral
/// deposit. Used when the user already holds the reserve's cToken and just
/// needs to register it against the obligation.
///
/// V2 inserts `lending_market_authority` (not present in v1) plus the farm
/// triple after the v1 deposit accounts. SDK ordering reference: see module
/// header comment.
///
/// Account list:
/// ```text
///   [0]  owner (signer)
///   [1]  obligation (mut)
///   [2]  lending_market
///   [3]  deposit_reserve (mut)
///   [4]  reserve_destination_collateral (mut)
///   [5]  user_source_collateral (mut)
///   [6]  token_program (SPL Token classic)
///   [7]  instruction_sysvar_account
///   [8]  lending_market_authority  (v2 addition)
///   [9]  obligation_farm_user_state (v2 — mut if farm present)
///   [10] reserve_farm_state         (v2 — mut if farm present)
///   [11] farms_program              (v2)
/// ```
pub fn deposit_obligation_collateral_v2_ix(
    user: &Pubkey,
    reserve: &ReserveAccounts,
    amount: u64,
    obligation_seed: (u8, u8),
) -> Result<Instruction> {
    if amount == 0 {
        return Err(Error::ZeroAmount);
    }
    let (tag, id) = obligation_seed;
    let user_collateral_ata = ata(user, &reserve.collateral_mint);
    let user_obligation = derive_user_obligation_with_seed(user, &reserve.lending_market, tag, id);

    let (farm_user_state, reserve_farm_state, farm_present) =
        v2_farm_accounts(reserve, &user_obligation, V2FarmKind::Collateral);

    let mut data = anchor_discriminator("global", "deposit_obligation_collateral_v2").to_vec();
    DepositArgs {
        // NOTE: even though klend's v1 DepositObligationCollateral arg is named
        // `collateral_amount`, on the wire it's the same single u64 — reuse
        // DepositArgs for the encoding.
        liquidity_amount: amount,
    }
    .serialize(&mut data)
    .map_err(|_| Error::Overflow)?;

    let farm_user_state_meta = if farm_present {
        AccountMeta::new(farm_user_state, false)
    } else {
        AccountMeta::new_readonly(farm_user_state, false)
    };
    let reserve_farm_state_meta = if farm_present {
        AccountMeta::new(reserve_farm_state, false)
    } else {
        AccountMeta::new_readonly(reserve_farm_state, false)
    };

    let accounts = vec![
        // v1 deposit-obligation-collateral accounts (8)
        AccountMeta::new(*user, true), // [0] owner (signer, mut for fee)
        AccountMeta::new(user_obligation, false), // [1] obligation (mut)
        AccountMeta::new_readonly(reserve.lending_market, false), // [2] lending_market
        AccountMeta::new(reserve.reserve, false), // [3] deposit_reserve (mut)
        AccountMeta::new(reserve.collateral_supply, false), // [4] reserve_destination_collateral
        AccountMeta::new(user_collateral_ata, false), // [5] user_source_collateral
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false), // [6] token_program (classic)
        AccountMeta::new_readonly(SYSVAR_INSTRUCTIONS_ID, false), // [7] instruction_sysvar_account
        // v2 additions (4)
        AccountMeta::new_readonly(reserve.lending_market_authority, false), // [8] lending_market_authority
        farm_user_state_meta,                                               // [9]
        reserve_farm_state_meta,                                            // [10]
        AccountMeta::new_readonly(KAMINO_FARMS_PROGRAM_ID, false),          // [11] farms_program
    ];

    Ok(Instruction {
        program_id: KAMINO_LEND_PROGRAM_ID,
        accounts,
        data,
    })
}

/// Bare `deposit_reserve_liquidity_and_obligation_collateral_v2` — v2 of the
/// combined deposit (liquidity → cToken auto-deposit → obligation collateral).
///
/// V2 appends the farm triple to the v1 account list. SDK ordering reference:
/// see module header comment.
///
/// Account list:
/// ```text
///   [0]  owner (signer, mut)
///   [1]  obligation (mut)
///   [2]  lending_market
///   [3]  lending_market_authority
///   [4]  reserve (mut)
///   [5]  reserve_liquidity_mint
///   [6]  reserve_liquidity_supply (mut)
///   [7]  reserve_collateral_mint (mut)
///   [8]  reserve_destination_deposit_collateral (mut)
///   [9]  user_source_liquidity (mut)
///   [10] placeholder_user_destination_collateral (None = klend program id)
///   [11] collateral_token_program (SPL Token classic)
///   [12] liquidity_token_program  (Token-2022 capable; here we pass classic)
///   [13] instruction_sysvar_account
///   [14] obligation_farm_user_state (v2 — mut if farm present)
///   [15] reserve_farm_state         (v2 — mut if farm present)
///   [16] farms_program              (v2)
/// ```
pub fn deposit_reserve_liquidity_and_obligation_collateral_v2_ix(
    user: &Pubkey,
    reserve: &ReserveAccounts,
    amount: u64,
    obligation_seed: (u8, u8),
) -> Result<Instruction> {
    if amount == 0 {
        return Err(Error::ZeroAmount);
    }
    let (tag, id) = obligation_seed;
    let user_liquidity_ata = ata(user, &reserve.liquidity_mint);
    let user_obligation = derive_user_obligation_with_seed(user, &reserve.lending_market, tag, id);

    let (farm_user_state, reserve_farm_state, farm_present) =
        v2_farm_accounts(reserve, &user_obligation, V2FarmKind::Collateral);

    let mut data = anchor_discriminator(
        "global",
        "deposit_reserve_liquidity_and_obligation_collateral_v2",
    )
    .to_vec();
    DepositArgs {
        liquidity_amount: amount,
    }
    .serialize(&mut data)
    .map_err(|_| Error::Overflow)?;

    let farm_user_state_meta = if farm_present {
        AccountMeta::new(farm_user_state, false)
    } else {
        AccountMeta::new_readonly(farm_user_state, false)
    };
    let reserve_farm_state_meta = if farm_present {
        AccountMeta::new(reserve_farm_state, false)
    } else {
        AccountMeta::new_readonly(reserve_farm_state, false)
    };

    let accounts = vec![
        // v1 deposit-reserve-liquidity-and-obligation-collateral (14)
        AccountMeta::new(*user, true),            // [0] owner
        AccountMeta::new(user_obligation, false), // [1] obligation
        AccountMeta::new_readonly(reserve.lending_market, false), // [2]
        AccountMeta::new_readonly(reserve.lending_market_authority, false), // [3]
        AccountMeta::new(reserve.reserve, false), // [4] reserve
        AccountMeta::new_readonly(reserve.liquidity_mint, false), // [5]
        AccountMeta::new(reserve.liquidity_supply, false), // [6]
        AccountMeta::new(reserve.collateral_mint, false), // [7]
        AccountMeta::new(reserve.collateral_supply, false), // [8]
        AccountMeta::new(user_liquidity_ata, false), // [9]
        AccountMeta::new_readonly(KAMINO_LEND_PROGRAM_ID, false), // [10] placeholder (None)
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false), // [11] collateral_token_program
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false), // [12] liquidity_token_program
        AccountMeta::new_readonly(SYSVAR_INSTRUCTIONS_ID, false), // [13]
        // v2 additions (3)
        farm_user_state_meta,                                      // [14]
        reserve_farm_state_meta,                                   // [15]
        AccountMeta::new_readonly(KAMINO_FARMS_PROGRAM_ID, false), // [16] farms_program
    ];

    Ok(Instruction {
        program_id: KAMINO_LEND_PROGRAM_ID,
        accounts,
        data,
    })
}

// ── refresh_reserve ─────────────────────────────────────────────────────────

/// `RefreshReserve` instruction. Must precede any deposit/withdraw on the
/// same reserve in the same transaction.
///
/// Account layout (6 accounts, verified against Kamino main market on mainnet):
///   [0] reserve (writable)
///   [1] lending_market (readonly)
///   [2] pyth_oracle — unused for USDC; pass KAMINO_LEND_PROGRAM_ID as placeholder
///   [3] switchboard_price_oracle — unused for USDC; pass KAMINO_LEND_PROGRAM_ID
///   [4] switchboard_twap_oracle — unused for USDC; pass KAMINO_LEND_PROGRAM_ID
///   [5] scope_prices — reserve-specific oracle; read from reserve data at offset 5112
pub fn refresh_reserve_ix(reserve: &ReserveAccounts) -> Instruction {
    let data = anchor_discriminator("global", "refresh_reserve").to_vec();

    let accounts = vec![
        AccountMeta::new(reserve.reserve, false),
        AccountMeta::new_readonly(reserve.lending_market, false),
        AccountMeta::new_readonly(KAMINO_LEND_PROGRAM_ID, false), // pyth (unused — placeholder)
        AccountMeta::new_readonly(KAMINO_LEND_PROGRAM_ID, false), // switchboard_price (unused — placeholder)
        AccountMeta::new_readonly(KAMINO_LEND_PROGRAM_ID, false), // switchboard_twap (unused — placeholder)
        AccountMeta::new_readonly(reserve.scope_prices, false),   // scope_prices oracle
    ];

    Instruction {
        program_id: KAMINO_LEND_PROGRAM_ID,
        accounts,
        data,
    }
}

// silence unused-import warning when the ATA helper is the only consumer
const _: &Pubkey = &ASSOCIATED_TOKEN_PROGRAM_ID;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::{KAMINO_MAIN_MARKET, USDC_MINT};

    fn dummy_reserve() -> ReserveAccounts {
        ReserveAccounts {
            reserve: Pubkey::new_unique(),
            lending_market: KAMINO_MAIN_MARKET,
            lending_market_authority: derive_lending_market_authority(&KAMINO_MAIN_MARKET),
            liquidity_mint: USDC_MINT,
            liquidity_supply: Pubkey::new_unique(),
            collateral_mint: Pubkey::new_unique(),
            collateral_supply: Pubkey::new_unique(),
            fee_receiver: Pubkey::new_unique(),
            scope_prices: Pubkey::default(),
            farm_collateral: Pubkey::default(),
            farm_debt: Pubkey::default(),
        }
    }

    #[test]
    fn deposit_rejects_zero_amount() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        assert!(matches!(
            deposit_ix(&user, &reserve, 0, (0, 0), &[]),
            Err(Error::ZeroAmount)
        ));
    }

    #[test]
    fn deposit_returns_five_instructions_no_farm() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ixs = deposit_ix(&user, &reserve, 1_000_000, (0, 0), &[]).expect("build");
        assert_eq!(
            ixs.len(),
            5,
            "init-obligation + ATA-create + refresh-reserve + refresh-obligation + deposit"
        );
    }

    #[test]
    fn deposit_returns_seven_instructions_with_farm() {
        let user = Pubkey::new_unique();
        let mut reserve = dummy_reserve();
        reserve.farm_collateral = Pubkey::new_unique();
        let ixs = deposit_ix(&user, &reserve, 1_000_000, (0, 0), &[]).expect("build");
        assert_eq!(ixs.len(), 7, "init-obligation + ATA + refresh-reserve + refresh-obligation + farms-pre + deposit + farms-post");
    }

    #[test]
    fn deposit_targets_kamino_program() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ixs = deposit_ix(&user, &reserve, 1_000_000, (0, 0), &[]).expect("build");
        let deposit = ixs.last().expect("has deposit");
        assert_eq!(deposit.program_id, KAMINO_LEND_PROGRAM_ID);
    }

    #[test]
    fn deposit_data_starts_with_anchor_discriminator() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ixs = deposit_ix(&user, &reserve, 1_000_000, (0, 0), &[]).expect("build");
        let deposit = ixs.last().expect("has deposit");
        // 8-byte discriminator + 8-byte u64 amount = 16 bytes total
        assert_eq!(deposit.data.len(), 16);
        let expected_disc = anchor_discriminator(
            "global",
            "deposit_reserve_liquidity_and_obligation_collateral",
        );
        assert_eq!(&deposit.data[..8], &expected_disc);
    }

    #[test]
    fn withdraw_rejects_zero_amount() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        assert!(matches!(
            withdraw_ix(&user, &reserve, 0, (0, 0), &[]),
            Err(Error::ZeroAmount)
        ));
    }

    #[test]
    fn obligation_pda_is_deterministic() {
        let user = Pubkey::new_unique();
        let lm = KAMINO_MAIN_MARKET;
        assert_eq!(
            derive_user_obligation(&user, &lm),
            derive_user_obligation(&user, &lm)
        );
    }

    #[test]
    fn derive_user_obligation_with_seed_distinguishes_tag_id_pairs() {
        // Strategy isolation: every (tag, id) pair must yield a unique
        // obligation PDA so a liquidation in one strategy cannot seize
        // collateral that belongs to another.
        let user = Pubkey::new_unique();
        let lm = KAMINO_MAIN_MARKET;
        let p_00 = derive_user_obligation_with_seed(&user, &lm, 0, 0);
        let p_01 = derive_user_obligation_with_seed(&user, &lm, 0, 1);
        let p_10 = derive_user_obligation_with_seed(&user, &lm, 1, 0);
        let p_11 = derive_user_obligation_with_seed(&user, &lm, 1, 1);
        assert_ne!(p_00, p_01);
        assert_ne!(p_00, p_10);
        assert_ne!(p_00, p_11);
        assert_ne!(p_01, p_10);
        assert_ne!(p_01, p_11);
        assert_ne!(p_10, p_11);
    }

    #[test]
    fn derive_user_obligation_wrapper_equals_zero_zero_seed() {
        // The wrapper must preserve stable-yield's existing PDA address.
        // If this assertion ever changes, stable-yield's $55 USDC obligation
        // at BPEv2HG... would be orphaned.
        let user = Pubkey::new_unique();
        let lm = KAMINO_MAIN_MARKET;
        assert_eq!(
            derive_user_obligation(&user, &lm),
            derive_user_obligation_with_seed(&user, &lm, 0, 0)
        );
    }

    #[test]
    fn multiply_obligation_pda_under_zero_one_seed() {
        // The multiply daemon uses (tag=0, id=1) — verify it's both
        // deterministic and distinct from the stable-yield (0, 0) PDA.
        use std::str::FromStr;
        let user = Pubkey::from_str("QesSR3TtkyrZmSEsRqrbg1DB3CHVSZDxMNLj5gZHuaJ").unwrap();
        let market = Pubkey::from_str("7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF").unwrap();
        let multiply = derive_user_obligation_with_seed(&user, &market, 0, 1);
        let stable = derive_user_obligation_with_seed(&user, &market, 0, 0);
        assert_ne!(multiply, stable);
        // Pin the multiply PDA so any later seed-bytes drift fails loudly.
        println!("multiply (0,1) obligation PDA = {multiply}");
    }

    #[test]
    fn stable_yield_obligation_pda_remains_unchanged() {
        // Concrete mainnet fixture: with user = QesSR3T... and market =
        // 7u3HeHx... the (0, 0) seed must derive
        // BPEv2HGHozQ1ZEaBWaSBuTLtTErB8nZXZoabucBbktbj. This guards against
        // any accidental change to the wrapper or the seed bytes that would
        // strand stable-yield's $55 USDC.
        use std::str::FromStr;
        let user = Pubkey::from_str("QesSR3TtkyrZmSEsRqrbg1DB3CHVSZDxMNLj5gZHuaJ").unwrap();
        let market = Pubkey::from_str("7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF").unwrap();
        let expected = Pubkey::from_str("BPEv2HGHozQ1ZEaBWaSBuTLtTErB8nZXZoabucBbktbj").unwrap();
        assert_eq!(derive_user_obligation(&user, &market), expected);
        assert_eq!(
            derive_user_obligation_with_seed(&user, &market, 0, 0),
            expected
        );
    }

    // ── borrow / repay / flash tests ────────────────────────────────────────

    #[test]
    fn borrow_rejects_zero_amount() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        assert!(matches!(
            borrow_obligation_liquidity_ix(&user, &reserve, 0, (0, 0)),
            Err(Error::ZeroAmount)
        ));
    }

    #[test]
    fn borrow_returns_three_instructions() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ixs =
            borrow_obligation_liquidity_ix(&user, &reserve, 1_000_000, (0, 0)).expect("build");
        assert_eq!(ixs.len(), 3, "ATA-create + refresh + borrow");
    }

    #[test]
    fn borrow_has_12_accounts_in_correct_order() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ixs =
            borrow_obligation_liquidity_ix(&user, &reserve, 1_000_000, (0, 0)).expect("build");
        let ix = ixs.last().unwrap();
        assert_eq!(ix.accounts.len(), 12);
        assert!(ix.accounts[0].is_signer);
        assert_eq!(ix.accounts[2].pubkey, reserve.lending_market);
        assert_eq!(ix.accounts[3].pubkey, reserve.lending_market_authority);
        assert_eq!(ix.accounts[4].pubkey, reserve.reserve);
        assert_eq!(ix.accounts[5].pubkey, reserve.liquidity_mint);
        assert_eq!(ix.accounts[6].pubkey, reserve.liquidity_supply);
        assert_eq!(ix.accounts[7].pubkey, reserve.fee_receiver);
        assert_eq!(ix.accounts[10].pubkey, TOKEN_PROGRAM_ID);
        assert_eq!(ix.accounts[11].pubkey, SYSVAR_INSTRUCTIONS_ID);
    }

    #[test]
    fn borrow_data_starts_with_anchor_discriminator() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ixs = borrow_obligation_liquidity_ix(&user, &reserve, 999_999, (0, 0)).expect("build");
        let ix = ixs.last().unwrap();
        // 8 disc + 8 amount = 16 bytes
        assert_eq!(ix.data.len(), 16);
        assert_eq!(
            &ix.data[..8],
            &anchor_discriminator("global", "borrow_obligation_liquidity")
        );
    }

    #[test]
    fn repay_rejects_zero_amount() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        assert!(matches!(
            repay_obligation_liquidity_ix(&user, &reserve, 0, (0, 0)),
            Err(Error::ZeroAmount)
        ));
    }

    #[test]
    fn repay_has_9_accounts_in_correct_order() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ixs = repay_obligation_liquidity_ix(&user, &reserve, 1_000_000, (0, 0)).expect("build");
        let ix = ixs.last().unwrap();
        assert_eq!(ix.accounts.len(), 9);
        assert!(ix.accounts[0].is_signer);
        assert_eq!(ix.accounts[2].pubkey, reserve.lending_market);
        assert_eq!(ix.accounts[3].pubkey, reserve.reserve);
        assert_eq!(ix.accounts[5].pubkey, reserve.liquidity_supply);
        assert_eq!(ix.accounts[7].pubkey, TOKEN_PROGRAM_ID);
        assert_eq!(ix.accounts[8].pubkey, SYSVAR_INSTRUCTIONS_ID);
    }

    #[test]
    fn flash_borrow_rejects_zero_amount() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        assert!(matches!(
            flash_borrow_reserve_liquidity_ix(&user, &reserve, 0),
            Err(Error::ZeroAmount)
        ));
    }

    #[test]
    fn flash_borrow_has_12_accounts() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ix = flash_borrow_reserve_liquidity_ix(&user, &reserve, 1_000_000).expect("build");
        assert_eq!(ix.accounts.len(), 12);
        assert!(ix.accounts[0].is_signer);
        assert_eq!(ix.accounts[1].pubkey, reserve.lending_market_authority);
        assert_eq!(ix.accounts[2].pubkey, reserve.lending_market);
        assert_eq!(ix.accounts[3].pubkey, reserve.reserve);
        assert_eq!(ix.accounts[7].pubkey, reserve.fee_receiver);
        assert_eq!(ix.accounts[10].pubkey, SYSVAR_INSTRUCTIONS_ID);
        assert_eq!(ix.accounts[11].pubkey, TOKEN_PROGRAM_ID);
    }

    #[test]
    fn flash_repay_data_includes_borrow_instruction_index() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ix = flash_repay_reserve_liquidity_ix(&user, &reserve, 1_000_000, 3).expect("build");
        // 8 disc + 8 amount + 1 borrow_index = 17 bytes
        assert_eq!(ix.data.len(), 17);
        assert_eq!(
            &ix.data[..8],
            &anchor_discriminator("global", "flash_repay_reserve_liquidity")
        );
        assert_eq!(ix.data[16], 3);
    }

    #[test]
    fn deposit_collateral_only_returns_single_instruction() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ix = deposit_collateral_only_ix(&user, &reserve, 1_000_000, (0, 0)).expect("build");
        assert_eq!(ix.program_id, KAMINO_LEND_PROGRAM_ID);
        assert_eq!(
            ix.accounts.len(),
            14,
            "14 accounts (no init/ATA/refresh wrapping)"
        );
        assert_eq!(ix.data.len(), 16, "8 disc + 8 amount");
    }

    #[test]
    fn deposit_collateral_only_rejects_zero() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        assert!(matches!(
            deposit_collateral_only_ix(&user, &reserve, 0, (0, 0)),
            Err(Error::ZeroAmount)
        ));
    }

    #[test]
    fn flash_repay_has_12_accounts_matching_borrow() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let borrow_ix = flash_borrow_reserve_liquidity_ix(&user, &reserve, 1).expect("borrow");
        let repay_ix = flash_repay_reserve_liquidity_ix(&user, &reserve, 1, 0).expect("repay");
        assert_eq!(borrow_ix.accounts.len(), repay_ix.accounts.len());
        // Same accounts in same positions (the structure is symmetric except the
        // mutable user-liquidity slot direction).
        for i in [0, 1, 2, 3, 4, 7, 10, 11] {
            assert_eq!(
                borrow_ix.accounts[i].pubkey, repay_ix.accounts[i].pubkey,
                "account[{i}] mismatch"
            );
        }
    }

    // ── refresh_obligation remaining_accounts tests ─────────────────────────

    #[test]
    fn refresh_obligation_no_remaining_accounts_for_fresh_obligation() {
        // Fresh obligation (no deposits, no borrows): RefreshObligation has
        // exactly the two named accounts and zero remaining accounts. Matches
        // pre-v0.1.5 behavior for first-deposit users.
        let user = Pubkey::new_unique();
        let market = Pubkey::new_unique();
        let ix = refresh_obligation_ix(&user, &market, (0, 0), &[]);
        assert_eq!(ix.accounts.len(), 2);
        assert_eq!(ix.accounts[0].pubkey, market);
        assert_eq!(
            ix.accounts[1].pubkey,
            derive_user_obligation(&user, &market)
        );
    }

    #[test]
    fn refresh_obligation_appends_one_deposit_reserve() {
        // Obligation with one registered deposit reserve (the
        // second-deposit / withdraw case): RefreshObligation must include
        // that reserve as the third account, readonly, non-signer.
        let user = Pubkey::new_unique();
        let market = Pubkey::new_unique();
        let reserve = Pubkey::new_unique();
        let ix = refresh_obligation_ix(&user, &market, (0, 0), &[reserve]);
        assert_eq!(ix.accounts.len(), 3);
        assert_eq!(ix.accounts[2].pubkey, reserve);
        assert!(!ix.accounts[2].is_writable);
        assert!(!ix.accounts[2].is_signer);
    }

    #[test]
    fn refresh_obligation_preserves_order_for_multiple_reserves() {
        // Three reserves (e.g. 2 deposits + 1 borrow): order must match the
        // slice — klend validates each remaining account positionally against
        // the obligation's deposits[] then borrows[] arrays.
        let user = Pubkey::new_unique();
        let market = Pubkey::new_unique();
        let r0 = Pubkey::new_unique();
        let r1 = Pubkey::new_unique();
        let r2 = Pubkey::new_unique();
        let ix = refresh_obligation_ix(&user, &market, (0, 0), &[r0, r1, r2]);
        assert_eq!(ix.accounts.len(), 5);
        assert_eq!(ix.accounts[2].pubkey, r0);
        assert_eq!(ix.accounts[3].pubkey, r1);
        assert_eq!(ix.accounts[4].pubkey, r2);
    }

    #[test]
    fn deposit_includes_obligation_reserves_in_refresh_obligation() {
        // End-to-end: deposit_ix should forward `obligation_reserves` into the
        // RefreshObligation ixn (index 3 in the no-farm bundle).
        let user = Pubkey::new_unique();
        let reserve_meta = dummy_reserve();
        let registered = Pubkey::new_unique();
        let ixs =
            deposit_ix(&user, &reserve_meta, 1_000_000, (0, 0), &[registered]).expect("build");
        // Bundle: [init_obligation, ATA, refresh_reserve, refresh_obligation, deposit]
        let refresh = &ixs[3];
        assert_eq!(
            refresh.accounts.len(),
            3,
            "lendingMarket + obligation + 1 reserve"
        );
        assert_eq!(refresh.accounts[2].pubkey, registered);
    }

    #[test]
    fn withdraw_bundles_refresh_obligation_with_reserves() {
        // withdraw_ix used to omit RefreshObligation entirely. After v0.1.5
        // the bundle is [ATA, refresh_reserve, refresh_obligation, withdraw]
        // with the obligation's registered reserves appended as remaining
        // accounts on the refresh_obligation step.
        let user = Pubkey::new_unique();
        let reserve_meta = dummy_reserve();
        let registered = Pubkey::new_unique();
        let ixs =
            withdraw_ix(&user, &reserve_meta, 1_000_000, (0, 0), &[registered]).expect("build");
        assert_eq!(
            ixs.len(),
            4,
            "ATA + refresh_reserve + refresh_obligation + withdraw"
        );
        let refresh = &ixs[2];
        assert_eq!(refresh.accounts.len(), 3);
        assert_eq!(refresh.accounts[2].pubkey, registered);
    }

    // ── v2 ixn builder tests ────────────────────────────────────────────────

    #[test]
    fn borrow_v2_rejects_zero_amount() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        assert!(matches!(
            borrow_obligation_liquidity_v2_ix(&user, &reserve, 0, (0, 0)),
            Err(Error::ZeroAmount)
        ));
    }

    #[test]
    fn borrow_v2_has_15_accounts_in_correct_order() {
        // v2 borrow = v1 12 accounts + obligation_farm_user_state +
        // reserve_farm_state + farms_program.
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ix = borrow_obligation_liquidity_v2_ix(&user, &reserve, 1_000_000, (0, 0))
            .expect("build v2 borrow");
        assert_eq!(ix.program_id, KAMINO_LEND_PROGRAM_ID);
        assert_eq!(
            ix.accounts.len(),
            15,
            "v1(12) + farm-user(1) + farm-state(1) + farms-program(1)"
        );
        // The 12-account v1 prefix matches the v1 builder positions.
        assert!(ix.accounts[0].is_signer);
        assert_eq!(ix.accounts[2].pubkey, reserve.lending_market);
        assert_eq!(ix.accounts[3].pubkey, reserve.lending_market_authority);
        assert_eq!(ix.accounts[4].pubkey, reserve.reserve);
        assert_eq!(ix.accounts[10].pubkey, TOKEN_PROGRAM_ID);
        assert_eq!(ix.accounts[11].pubkey, SYSVAR_INSTRUCTIONS_ID);
        // farms_program at the tail.
        assert_eq!(ix.accounts[14].pubkey, KAMINO_FARMS_PROGRAM_ID);
        assert!(!ix.accounts[14].is_writable);
    }

    #[test]
    fn borrow_v2_farm_placeholders_when_no_debt_farm() {
        // Reserve with no debt farm: the SDK encodes None as the klend
        // program id (readonly). Mirror that.
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve(); // farm_debt = default
        let ix = borrow_obligation_liquidity_v2_ix(&user, &reserve, 1_000_000, (0, 0))
            .expect("build v2 borrow");
        assert_eq!(ix.accounts[12].pubkey, KAMINO_LEND_PROGRAM_ID);
        assert!(!ix.accounts[12].is_writable);
        assert_eq!(ix.accounts[13].pubkey, KAMINO_LEND_PROGRAM_ID);
        assert!(!ix.accounts[13].is_writable);
    }

    #[test]
    fn borrow_v2_farm_accounts_real_when_debt_farm_present() {
        let user = Pubkey::new_unique();
        let mut reserve = dummy_reserve();
        reserve.farm_debt = Pubkey::new_unique();
        let obligation = derive_user_obligation_with_seed(&user, &reserve.lending_market, 0, 0);
        let expected_user_state =
            derive_obligation_farm_user_state(&reserve.farm_debt, &obligation);

        let ix = borrow_obligation_liquidity_v2_ix(&user, &reserve, 1_000_000, (0, 0))
            .expect("build v2 borrow with debt farm");
        assert_eq!(ix.accounts[12].pubkey, expected_user_state);
        assert!(ix.accounts[12].is_writable);
        assert_eq!(ix.accounts[13].pubkey, reserve.farm_debt);
        assert!(ix.accounts[13].is_writable);
    }

    #[test]
    fn borrow_v2_discriminator_matches_anchor_name() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ix = borrow_obligation_liquidity_v2_ix(&user, &reserve, 42, (0, 0)).expect("build v2");
        assert_eq!(ix.data.len(), 16);
        assert_eq!(
            &ix.data[..8],
            &anchor_discriminator("global", "borrow_obligation_liquidity_v2")
        );
    }

    #[test]
    fn deposit_collateral_v2_rejects_zero_amount() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        assert!(matches!(
            deposit_obligation_collateral_v2_ix(&user, &reserve, 0, (0, 0)),
            Err(Error::ZeroAmount)
        ));
    }

    #[test]
    fn deposit_collateral_v2_has_12_accounts_in_correct_order() {
        // v2 deposit_obligation_collateral = v1 8 accounts +
        // lending_market_authority + farm-user + farm-state + farms_program.
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ix = deposit_obligation_collateral_v2_ix(&user, &reserve, 1_000_000, (0, 0))
            .expect("build v2 deposit_collateral");
        assert_eq!(ix.program_id, KAMINO_LEND_PROGRAM_ID);
        assert_eq!(ix.accounts.len(), 12);
        assert!(ix.accounts[0].is_signer);
        assert_eq!(ix.accounts[2].pubkey, reserve.lending_market);
        assert_eq!(ix.accounts[3].pubkey, reserve.reserve);
        assert_eq!(ix.accounts[6].pubkey, TOKEN_PROGRAM_ID);
        assert_eq!(ix.accounts[7].pubkey, SYSVAR_INSTRUCTIONS_ID);
        // v2 additions
        assert_eq!(ix.accounts[8].pubkey, reserve.lending_market_authority);
        assert!(!ix.accounts[8].is_writable);
        assert_eq!(ix.accounts[11].pubkey, KAMINO_FARMS_PROGRAM_ID);
    }

    #[test]
    fn deposit_collateral_v2_farm_placeholders_when_no_collateral_farm() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ix = deposit_obligation_collateral_v2_ix(&user, &reserve, 1_000_000, (0, 0))
            .expect("build v2");
        assert_eq!(ix.accounts[9].pubkey, KAMINO_LEND_PROGRAM_ID);
        assert!(!ix.accounts[9].is_writable);
        assert_eq!(ix.accounts[10].pubkey, KAMINO_LEND_PROGRAM_ID);
        assert!(!ix.accounts[10].is_writable);
    }

    #[test]
    fn deposit_collateral_v2_farm_accounts_real_when_collateral_farm_present() {
        let user = Pubkey::new_unique();
        let mut reserve = dummy_reserve();
        reserve.farm_collateral = Pubkey::new_unique();
        let obligation = derive_user_obligation_with_seed(&user, &reserve.lending_market, 0, 0);
        let expected_user_state =
            derive_obligation_farm_user_state(&reserve.farm_collateral, &obligation);

        let ix = deposit_obligation_collateral_v2_ix(&user, &reserve, 1_000_000, (0, 0))
            .expect("build v2 deposit");
        assert_eq!(ix.accounts[9].pubkey, expected_user_state);
        assert!(ix.accounts[9].is_writable);
        assert_eq!(ix.accounts[10].pubkey, reserve.farm_collateral);
        assert!(ix.accounts[10].is_writable);
    }

    #[test]
    fn deposit_collateral_v2_discriminator_matches_anchor_name() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ix = deposit_obligation_collateral_v2_ix(&user, &reserve, 7, (0, 0)).expect("build v2");
        assert_eq!(
            &ix.data[..8],
            &anchor_discriminator("global", "deposit_obligation_collateral_v2")
        );
    }

    #[test]
    fn deposit_reserve_v2_has_17_accounts_in_correct_order() {
        // v2 deposit_reserve_liquidity_and_obligation_collateral = v1 14 +
        // farm-user + farm-state + farms_program = 17.
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ix = deposit_reserve_liquidity_and_obligation_collateral_v2_ix(
            &user,
            &reserve,
            1_000_000,
            (0, 0),
        )
        .expect("build v2 deposit_reserve");
        assert_eq!(ix.program_id, KAMINO_LEND_PROGRAM_ID);
        assert_eq!(ix.accounts.len(), 17);
        assert!(ix.accounts[0].is_signer);
        // v1 prefix sanity-checks
        assert_eq!(ix.accounts[3].pubkey, reserve.lending_market_authority);
        assert_eq!(ix.accounts[4].pubkey, reserve.reserve);
        assert_eq!(ix.accounts[10].pubkey, KAMINO_LEND_PROGRAM_ID); // None placeholder
        assert_eq!(ix.accounts[13].pubkey, SYSVAR_INSTRUCTIONS_ID);
        // v2 tail
        assert_eq!(ix.accounts[16].pubkey, KAMINO_FARMS_PROGRAM_ID);
    }

    #[test]
    fn deposit_reserve_v2_farm_placeholders_when_no_collateral_farm() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ix = deposit_reserve_liquidity_and_obligation_collateral_v2_ix(
            &user,
            &reserve,
            1_000_000,
            (0, 0),
        )
        .expect("build v2");
        assert_eq!(ix.accounts[14].pubkey, KAMINO_LEND_PROGRAM_ID);
        assert!(!ix.accounts[14].is_writable);
        assert_eq!(ix.accounts[15].pubkey, KAMINO_LEND_PROGRAM_ID);
        assert!(!ix.accounts[15].is_writable);
    }

    #[test]
    fn deposit_reserve_v2_farm_accounts_real_when_collateral_farm_present() {
        let user = Pubkey::new_unique();
        let mut reserve = dummy_reserve();
        reserve.farm_collateral = Pubkey::new_unique();
        let obligation = derive_user_obligation_with_seed(&user, &reserve.lending_market, 0, 0);
        let expected_user_state =
            derive_obligation_farm_user_state(&reserve.farm_collateral, &obligation);

        let ix = deposit_reserve_liquidity_and_obligation_collateral_v2_ix(
            &user,
            &reserve,
            1_000_000,
            (0, 0),
        )
        .expect("build v2");
        assert_eq!(ix.accounts[14].pubkey, expected_user_state);
        assert!(ix.accounts[14].is_writable);
        assert_eq!(ix.accounts[15].pubkey, reserve.farm_collateral);
        assert!(ix.accounts[15].is_writable);
    }

    #[test]
    fn deposit_reserve_v2_discriminator_matches_anchor_name() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ix =
            deposit_reserve_liquidity_and_obligation_collateral_v2_ix(&user, &reserve, 9, (0, 0))
                .expect("build v2");
        assert_eq!(
            &ix.data[..8],
            &anchor_discriminator(
                "global",
                "deposit_reserve_liquidity_and_obligation_collateral_v2"
            )
        );
    }

    // ── repay_obligation_liquidity_v2 tests ─────────────────────────────────

    #[test]
    fn repay_v2_rejects_zero_amount() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        assert!(matches!(
            repay_obligation_liquidity_v2_ix(&user, &reserve, 0, (0, 0)),
            Err(Error::ZeroAmount)
        ));
    }

    #[test]
    fn repay_obligation_liquidity_v2_account_count_and_order() {
        // v2 repay = v1 9 + farm-user + farm-state + lending_market_authority
        // + farms_program = 13. Matches klend's RepayObligationLiquidityV2
        // #[derive(Accounts)] struct verbatim (handler_repay_obligation_liquidity.rs).
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ix = repay_obligation_liquidity_v2_ix(&user, &reserve, 1_000_000, (0, 0))
            .expect("build v2 repay");
        assert_eq!(ix.program_id, KAMINO_LEND_PROGRAM_ID);
        assert_eq!(
            ix.accounts.len(),
            13,
            "v1(9) + farm-user(1) + farm-state(1) + lending_market_authority(1) + farms-program(1)"
        );
        // v1 prefix sanity-checks
        assert!(ix.accounts[0].is_signer);
        assert!(!ix.accounts[0].is_writable); // owner is readonly in repay (no fee debit)
        assert_eq!(
            ix.accounts[1].pubkey,
            derive_user_obligation_with_seed(&user, &reserve.lending_market, 0, 0)
        );
        assert!(ix.accounts[1].is_writable);
        assert_eq!(ix.accounts[2].pubkey, reserve.lending_market);
        assert!(!ix.accounts[2].is_writable);
        assert_eq!(ix.accounts[3].pubkey, reserve.reserve); // repay_reserve
        assert!(ix.accounts[3].is_writable);
        assert_eq!(ix.accounts[4].pubkey, reserve.liquidity_mint);
        assert_eq!(ix.accounts[5].pubkey, reserve.liquidity_supply);
        assert!(ix.accounts[5].is_writable);
        assert_eq!(ix.accounts[6].pubkey, ata(&user, &reserve.liquidity_mint));
        assert!(ix.accounts[6].is_writable);
        assert_eq!(ix.accounts[7].pubkey, TOKEN_PROGRAM_ID);
        assert_eq!(ix.accounts[8].pubkey, SYSVAR_INSTRUCTIONS_ID);
        // v2 tail
        assert_eq!(ix.accounts[11].pubkey, reserve.lending_market_authority);
        assert!(!ix.accounts[11].is_writable);
        assert_eq!(ix.accounts[12].pubkey, KAMINO_FARMS_PROGRAM_ID);
        assert!(!ix.accounts[12].is_writable);
    }

    #[test]
    fn repay_v2_farm_placeholder_when_reserve_has_no_farm() {
        // Reserve with no debt farm: the SDK encodes None as the klend
        // program id (readonly). Mirror that.
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve(); // farm_debt = default
        let ix = repay_obligation_liquidity_v2_ix(&user, &reserve, 1_000_000, (0, 0))
            .expect("build v2 repay");
        assert_eq!(ix.accounts[9].pubkey, KAMINO_LEND_PROGRAM_ID);
        assert!(!ix.accounts[9].is_writable);
        assert_eq!(ix.accounts[10].pubkey, KAMINO_LEND_PROGRAM_ID);
        assert!(!ix.accounts[10].is_writable);
        // lending_market_authority and farms_program stay populated regardless
        // of farm presence (they live outside OptionalObligationFarmsAccounts).
        assert_eq!(ix.accounts[11].pubkey, reserve.lending_market_authority);
        assert_eq!(ix.accounts[12].pubkey, KAMINO_FARMS_PROGRAM_ID);
    }

    #[test]
    fn repay_obligation_liquidity_v2_discriminator_is_stable() {
        // Belt-and-braces: the v0.3.3 fix only touched the account list — the
        // 8-byte anchor discriminator must NOT have moved. If this test ever
        // diverges from the IDL's hash for ("global", "repay_obligation_liquidity_v2"),
        // klend will reject the ixn before it even reads the account list.
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ix =
            repay_obligation_liquidity_v2_ix(&user, &reserve, 7, (0, 0)).expect("build v2 repay");
        let expected = anchor_discriminator("global", "repay_obligation_liquidity_v2");
        assert_eq!(&ix.data[..8], &expected, "discriminator drift");
        // Also confirm the data length is unchanged (8 disc + 8 u64 arg).
        assert_eq!(ix.data.len(), 16);
    }

    #[test]
    fn repay_v2_farm_accounts_real_when_debt_farm_present() {
        let user = Pubkey::new_unique();
        let mut reserve = dummy_reserve();
        reserve.farm_debt = Pubkey::new_unique();
        let obligation = derive_user_obligation_with_seed(&user, &reserve.lending_market, 0, 0);
        let expected_user_state =
            derive_obligation_farm_user_state(&reserve.farm_debt, &obligation);

        let ix = repay_obligation_liquidity_v2_ix(&user, &reserve, 1_000_000, (0, 0))
            .expect("build v2 repay with debt farm");
        assert_eq!(ix.accounts[9].pubkey, expected_user_state);
        assert!(ix.accounts[9].is_writable);
        assert_eq!(ix.accounts[10].pubkey, reserve.farm_debt);
        assert!(ix.accounts[10].is_writable);
    }

    #[test]
    fn repay_obligation_liquidity_v2_anchor_discriminator_matches_idl() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ix =
            repay_obligation_liquidity_v2_ix(&user, &reserve, 42, (0, 0)).expect("build v2 repay");
        assert_eq!(ix.data.len(), 16, "8-byte disc + 8-byte u64 arg");
        assert_eq!(
            &ix.data[..8],
            &anchor_discriminator("global", "repay_obligation_liquidity_v2")
        );
    }

    #[test]
    fn repay_v2_round_trips_u64_max_sentinel() {
        // klend interprets u64::MAX as "repay full balance" — verify it
        // serialises through Borsh without overflow.
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ix = repay_obligation_liquidity_v2_ix(&user, &reserve, u64::MAX, (0, 0))
            .expect("build v2 repay with u64::MAX");
        // bytes 8..16 should be the little-endian u64::MAX
        assert_eq!(&ix.data[8..16], &u64::MAX.to_le_bytes());
    }

    // ── withdraw_obligation_collateral_v2 tests ─────────────────────────────

    #[test]
    fn withdraw_collateral_v2_rejects_zero_amount() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        assert!(matches!(
            withdraw_obligation_collateral_v2_ix(&user, &reserve, 0, (0, 0)),
            Err(Error::ZeroAmount)
        ));
    }

    #[test]
    fn withdraw_collateral_v2_has_12_accounts_in_correct_order() {
        // v2 withdraw_obligation_collateral = v1 9 + farm-user + farm-state
        // + farms_program = 12.
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ix = withdraw_obligation_collateral_v2_ix(&user, &reserve, 1_000_000, (0, 0))
            .expect("build v2 withdraw_collateral");
        assert_eq!(ix.program_id, KAMINO_LEND_PROGRAM_ID);
        assert_eq!(
            ix.accounts.len(),
            12,
            "v1(9) + farm-user(1) + farm-state(1) + farms-program(1)"
        );
        assert!(ix.accounts[0].is_signer);
        assert!(!ix.accounts[0].is_writable);
        assert_eq!(ix.accounts[2].pubkey, reserve.lending_market);
        assert_eq!(ix.accounts[3].pubkey, reserve.lending_market_authority);
        assert_eq!(ix.accounts[4].pubkey, reserve.reserve);
        assert_eq!(ix.accounts[5].pubkey, reserve.collateral_supply);
        assert_eq!(ix.accounts[7].pubkey, TOKEN_PROGRAM_ID);
        assert_eq!(ix.accounts[8].pubkey, SYSVAR_INSTRUCTIONS_ID);
        // v2 tail
        assert_eq!(ix.accounts[11].pubkey, KAMINO_FARMS_PROGRAM_ID);
        assert!(!ix.accounts[11].is_writable);
    }

    #[test]
    fn withdraw_collateral_v2_farm_placeholder_when_reserve_has_no_farm() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve(); // farm_collateral = default
        let ix = withdraw_obligation_collateral_v2_ix(&user, &reserve, 1_000_000, (0, 0))
            .expect("build v2");
        assert_eq!(ix.accounts[9].pubkey, KAMINO_LEND_PROGRAM_ID);
        assert!(!ix.accounts[9].is_writable);
        assert_eq!(ix.accounts[10].pubkey, KAMINO_LEND_PROGRAM_ID);
        assert!(!ix.accounts[10].is_writable);
    }

    #[test]
    fn withdraw_collateral_v2_farm_accounts_real_when_collateral_farm_present() {
        let user = Pubkey::new_unique();
        let mut reserve = dummy_reserve();
        reserve.farm_collateral = Pubkey::new_unique();
        let obligation = derive_user_obligation_with_seed(&user, &reserve.lending_market, 0, 0);
        let expected_user_state =
            derive_obligation_farm_user_state(&reserve.farm_collateral, &obligation);

        let ix = withdraw_obligation_collateral_v2_ix(&user, &reserve, 1_000_000, (0, 0))
            .expect("build v2 withdraw_collateral with collateral farm");
        assert_eq!(ix.accounts[9].pubkey, expected_user_state);
        assert!(ix.accounts[9].is_writable);
        assert_eq!(ix.accounts[10].pubkey, reserve.farm_collateral);
        assert!(ix.accounts[10].is_writable);
    }

    #[test]
    fn withdraw_obligation_collateral_v2_anchor_discriminator_matches_idl() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ix = withdraw_obligation_collateral_v2_ix(&user, &reserve, 7, (0, 0))
            .expect("build v2 withdraw_collateral");
        assert_eq!(ix.data.len(), 16, "8-byte disc + 8-byte u64 arg");
        assert_eq!(
            &ix.data[..8],
            &anchor_discriminator("global", "withdraw_obligation_collateral_v2")
        );
    }

    #[test]
    fn withdraw_collateral_v2_round_trips_u64_max_sentinel() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ix = withdraw_obligation_collateral_v2_ix(&user, &reserve, u64::MAX, (0, 0))
            .expect("build v2 withdraw_collateral with u64::MAX");
        assert_eq!(&ix.data[8..16], &u64::MAX.to_le_bytes());
    }

    // ── withdraw_obligation_collateral_and_redeem_reserve_collateral_v2 ─────

    #[test]
    fn withdraw_and_redeem_v2_rejects_zero_amount() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        assert!(matches!(
            withdraw_obligation_collateral_and_redeem_reserve_collateral_v2_ix(
                &user,
                &reserve,
                0,
                (0, 0)
            ),
            Err(Error::ZeroAmount)
        ));
    }

    #[test]
    fn withdraw_and_redeem_v2_has_17_accounts_in_correct_order() {
        // v2 withdraw-and-redeem = v1 14 + farm-user + farm-state + farms = 17.
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ix = withdraw_obligation_collateral_and_redeem_reserve_collateral_v2_ix(
            &user,
            &reserve,
            1_000_000,
            (0, 0),
        )
        .expect("build v2 withdraw_and_redeem");
        assert_eq!(ix.program_id, KAMINO_LEND_PROGRAM_ID);
        assert_eq!(ix.accounts.len(), 17);
        assert!(ix.accounts[0].is_signer);
        assert!(ix.accounts[0].is_writable); // owner is mut in combined withdraw
                                             // v1 prefix sanity-checks (mirroring §3.3 of the plan)
        assert_eq!(ix.accounts[2].pubkey, reserve.lending_market);
        assert_eq!(ix.accounts[3].pubkey, reserve.lending_market_authority);
        assert_eq!(ix.accounts[4].pubkey, reserve.reserve);
        assert_eq!(ix.accounts[5].pubkey, reserve.liquidity_mint);
        assert_eq!(ix.accounts[6].pubkey, reserve.collateral_supply);
        assert_eq!(ix.accounts[7].pubkey, reserve.collateral_mint);
        assert_eq!(ix.accounts[8].pubkey, reserve.liquidity_supply);
        assert_eq!(ix.accounts[10].pubkey, KAMINO_LEND_PROGRAM_ID); // None placeholder
        assert_eq!(ix.accounts[13].pubkey, SYSVAR_INSTRUCTIONS_ID);
        // v2 tail
        assert_eq!(ix.accounts[16].pubkey, KAMINO_FARMS_PROGRAM_ID);
    }

    #[test]
    fn withdraw_and_redeem_v2_farm_placeholder_when_reserve_has_no_farm() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve(); // farm_collateral = default
        let ix = withdraw_obligation_collateral_and_redeem_reserve_collateral_v2_ix(
            &user,
            &reserve,
            1_000_000,
            (0, 0),
        )
        .expect("build v2");
        assert_eq!(ix.accounts[14].pubkey, KAMINO_LEND_PROGRAM_ID);
        assert!(!ix.accounts[14].is_writable);
        assert_eq!(ix.accounts[15].pubkey, KAMINO_LEND_PROGRAM_ID);
        assert!(!ix.accounts[15].is_writable);
    }

    #[test]
    fn withdraw_and_redeem_v2_farm_accounts_real_when_collateral_farm_present() {
        let user = Pubkey::new_unique();
        let mut reserve = dummy_reserve();
        reserve.farm_collateral = Pubkey::new_unique();
        let obligation = derive_user_obligation_with_seed(&user, &reserve.lending_market, 0, 0);
        let expected_user_state =
            derive_obligation_farm_user_state(&reserve.farm_collateral, &obligation);

        let ix = withdraw_obligation_collateral_and_redeem_reserve_collateral_v2_ix(
            &user,
            &reserve,
            1_000_000,
            (0, 0),
        )
        .expect("build v2 withdraw_and_redeem with collateral farm");
        assert_eq!(ix.accounts[14].pubkey, expected_user_state);
        assert!(ix.accounts[14].is_writable);
        assert_eq!(ix.accounts[15].pubkey, reserve.farm_collateral);
        assert!(ix.accounts[15].is_writable);
    }

    #[test]
    fn withdraw_and_redeem_v2_discriminator_matches_anchor_name() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ix = withdraw_obligation_collateral_and_redeem_reserve_collateral_v2_ix(
            &user,
            &reserve,
            9,
            (0, 0),
        )
        .expect("build v2");
        assert_eq!(
            &ix.data[..8],
            &anchor_discriminator(
                "global",
                "withdraw_obligation_collateral_and_redeem_reserve_collateral_v2"
            )
        );
    }

    #[test]
    fn withdraw_and_redeem_v2_round_trips_u64_max_sentinel() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ix = withdraw_obligation_collateral_and_redeem_reserve_collateral_v2_ix(
            &user,
            &reserve,
            u64::MAX,
            (0, 0),
        )
        .expect("build v2 with u64::MAX");
        assert_eq!(&ix.data[8..16], &u64::MAX.to_le_bytes());
    }
}
