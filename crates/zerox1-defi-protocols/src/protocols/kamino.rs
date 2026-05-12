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
}

/// Derive the lending market authority PDA for a given lending market.
pub fn derive_lending_market_authority(lending_market: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"lma", lending_market.as_ref()], &KAMINO_LEND_PROGRAM_ID).0
}

/// Derive the user obligation PDA for a given lending market + user.
///
/// Klend uses a tag + id system to support multiple obligations per user.
/// For our use case (single obligation per user per market) we use tag=0, id=0.
pub fn derive_user_obligation(user: &Pubkey, lending_market: &Pubkey) -> Pubkey {
    let tag: u8 = 0;
    let id: u8 = 0;
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
pub fn initialize_obligation_ix(user: &Pubkey, lending_market: &Pubkey) -> Result<Instruction> {
    let obligation = derive_user_obligation(user, lending_market);
    let user_metadata = derive_user_metadata(user);

    // Discriminator: sha256("global:init_obligation")[0..8] = fb0ae74c1b0b9f60
    let mut data = anchor_discriminator("global", "init_obligation").to_vec();
    InitObligationArgs { tag: 0, id: 0 }
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
pub fn refresh_obligation_ix(user: &Pubkey, lending_market: &Pubkey) -> Instruction {
    let obligation = derive_user_obligation(user, lending_market);
    let data = anchor_discriminator("global", "refresh_obligation").to_vec();
    Instruction {
        program_id: KAMINO_LEND_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(*lending_market, false), // lendingMarket
            AccountMeta::new(obligation, false),               // obligation (writable)
        ],
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
) -> Instruction {
    let obligation = derive_user_obligation(user, &reserve.lending_market);
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
) -> Instruction {
    let obligation = derive_user_obligation(user, &reserve.lending_market);
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
) -> Result<Vec<Instruction>> {
    if amount == 0 {
        return Err(Error::ZeroAmount);
    }

    let user_liquidity_ata = ata(user, &reserve.liquidity_mint);
    let user_obligation = derive_user_obligation(user, &reserve.lending_market);
    let has_farm = reserve.farm_collateral != Pubkey::default();

    let mut ixs = Vec::with_capacity(if has_farm { 7 } else { 5 });

    // 1. InitializeObligation (no-op if the obligation already exists).
    ixs.push(initialize_obligation_ix(user, &reserve.lending_market)?);

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
    ixs.push(refresh_obligation_ix(user, &reserve.lending_market));

    // 5. RefreshObligationFarmsForReserve (pre) — at deposit-1.
    // check_refresh also requires a matching instruction at deposit+1 (post),
    // which is appended after the deposit below.
    if has_farm {
        ixs.push(refresh_obligation_farms_for_reserve_ix(user, user, reserve));
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
        ixs.push(refresh_obligation_farms_for_reserve_ix(user, user, reserve));
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
) -> Result<Instruction> {
    if amount == 0 {
        return Err(Error::ZeroAmount);
    }
    let user_liquidity_ata = ata(user, &reserve.liquidity_mint);
    let user_obligation = derive_user_obligation(user, &reserve.lending_market);

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
) -> Result<Vec<Instruction>> {
    if amount == 0 {
        return Err(Error::ZeroAmount);
    }

    let user_liquidity_ata = ata(user, &reserve.liquidity_mint);
    let user_obligation = derive_user_obligation(user, &reserve.lending_market);

    let mut ixs = Vec::with_capacity(3);

    ixs.push(create_associated_token_account_idempotent(
        user,
        user,
        &reserve.liquidity_mint,
        &TOKEN_PROGRAM_ID,
    ));

    ixs.push(refresh_reserve_ix(reserve));

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
) -> Result<Vec<Instruction>> {
    if amount == 0 {
        return Err(Error::ZeroAmount);
    }

    let user_destination_ata = ata(user, &reserve.liquidity_mint);
    let user_obligation = derive_user_obligation(user, &reserve.lending_market);

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
) -> Result<Vec<Instruction>> {
    if amount == 0 {
        return Err(Error::ZeroAmount);
    }

    let user_source_ata = ata(user, &reserve.liquidity_mint);
    let user_obligation = derive_user_obligation(user, &reserve.lending_market);

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
        }
    }

    #[test]
    fn deposit_rejects_zero_amount() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        assert!(matches!(
            deposit_ix(&user, &reserve, 0),
            Err(Error::ZeroAmount)
        ));
    }

    #[test]
    fn deposit_returns_five_instructions_no_farm() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ixs = deposit_ix(&user, &reserve, 1_000_000).expect("build");
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
        let ixs = deposit_ix(&user, &reserve, 1_000_000).expect("build");
        assert_eq!(ixs.len(), 7, "init-obligation + ATA + refresh-reserve + refresh-obligation + farms-pre + deposit + farms-post");
    }

    #[test]
    fn deposit_targets_kamino_program() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ixs = deposit_ix(&user, &reserve, 1_000_000).expect("build");
        let deposit = ixs.last().expect("has deposit");
        assert_eq!(deposit.program_id, KAMINO_LEND_PROGRAM_ID);
    }

    #[test]
    fn deposit_data_starts_with_anchor_discriminator() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ixs = deposit_ix(&user, &reserve, 1_000_000).expect("build");
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
            withdraw_ix(&user, &reserve, 0),
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

    // ── borrow / repay / flash tests ────────────────────────────────────────

    #[test]
    fn borrow_rejects_zero_amount() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        assert!(matches!(
            borrow_obligation_liquidity_ix(&user, &reserve, 0),
            Err(Error::ZeroAmount)
        ));
    }

    #[test]
    fn borrow_returns_three_instructions() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ixs = borrow_obligation_liquidity_ix(&user, &reserve, 1_000_000).expect("build");
        assert_eq!(ixs.len(), 3, "ATA-create + refresh + borrow");
    }

    #[test]
    fn borrow_has_12_accounts_in_correct_order() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ixs = borrow_obligation_liquidity_ix(&user, &reserve, 1_000_000).expect("build");
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
        let ixs = borrow_obligation_liquidity_ix(&user, &reserve, 999_999).expect("build");
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
            repay_obligation_liquidity_ix(&user, &reserve, 0),
            Err(Error::ZeroAmount)
        ));
    }

    #[test]
    fn repay_has_9_accounts_in_correct_order() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ixs = repay_obligation_liquidity_ix(&user, &reserve, 1_000_000).expect("build");
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
        let ix = deposit_collateral_only_ix(&user, &reserve, 1_000_000).expect("build");
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
            deposit_collateral_only_ix(&user, &reserve, 0),
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
}
