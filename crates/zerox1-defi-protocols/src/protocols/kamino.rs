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
        ASSOCIATED_TOKEN_PROGRAM_ID, KAMINO_LEND_PROGRAM_ID, SYSTEM_PROGRAM_ID,
        SYSVAR_INSTRUCTIONS_ID, TOKEN_PROGRAM_ID,
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
}

/// Derive the lending market authority PDA for a given lending market.
pub fn derive_lending_market_authority(lending_market: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"lma", lending_market.as_ref()],
        &KAMINO_LEND_PROGRAM_ID,
    )
    .0
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

// ── Instruction data structures ─────────────────────────────────────────────

#[derive(BorshSerialize)]
struct DepositArgs {
    liquidity_amount: u64,
}

#[derive(BorshSerialize)]
struct WithdrawArgs {
    collateral_amount: u64,
}

// ── deposit_reserve_liquidity_and_obligation_collateral ─────────────────────

/// Build the instruction sequence to deposit `amount` raw units of liquidity
/// (e.g. 1_000_000 = 1 USDC) into a Kamino reserve and credit the user's
/// obligation with the corresponding cTokens.
///
/// Returns:
/// 1. Idempotent ATA-create for the user's liquidity ATA (no-op if exists)
/// 2. RefreshReserve (required before any deposit/withdraw)
/// 3. The deposit instruction itself
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

    let mut ixs = Vec::with_capacity(3);

    // 1. Idempotent ATA-create for liquidity (no-op if exists).
    ixs.push(create_associated_token_account_idempotent(
        user,
        user,
        &reserve.liquidity_mint,
        &TOKEN_PROGRAM_ID,
    ));

    // 2. RefreshReserve.
    ixs.push(refresh_reserve_ix(reserve));

    // 3. Deposit.
    let mut data = anchor_discriminator("global", "deposit_reserve_liquidity_and_obligation_collateral").to_vec();
    DepositArgs { liquidity_amount: amount }
        .serialize(&mut data)
        .map_err(|_| Error::Overflow)?;

    let accounts = vec![
        AccountMeta::new(*user, true),                              // owner (signer)
        AccountMeta::new(user_obligation, false),                   // obligation
        AccountMeta::new_readonly(reserve.lending_market, false),   // lending_market
        AccountMeta::new_readonly(reserve.lending_market_authority, false),
        AccountMeta::new(reserve.reserve, false),                   // reserve
        AccountMeta::new_readonly(reserve.liquidity_mint, false),   // reserve_liquidity_mint
        AccountMeta::new(reserve.liquidity_supply, false),          // reserve_liquidity_supply
        AccountMeta::new(reserve.collateral_mint, false),           // reserve_collateral_mint
        AccountMeta::new(reserve.collateral_supply, false),         // reserve_destination_deposit_collateral
        AccountMeta::new(user_liquidity_ata, false),                // user_source_liquidity
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),         // collateral_token_program
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),         // liquidity_token_program
        AccountMeta::new_readonly(SYSVAR_INSTRUCTIONS_ID, false),   // instruction_sysvar
    ];

    ixs.push(Instruction {
        program_id: KAMINO_LEND_PROGRAM_ID,
        accounts,
        data,
    });

    Ok(ixs)
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
    WithdrawArgs { collateral_amount: amount }
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

// ── refresh_reserve ─────────────────────────────────────────────────────────

/// `RefreshReserve` instruction. Must precede any deposit/withdraw on the
/// same reserve in the same transaction.
pub fn refresh_reserve_ix(reserve: &ReserveAccounts) -> Instruction {
    let data = anchor_discriminator("global", "refresh_reserve").to_vec();

    let accounts = vec![
        AccountMeta::new(reserve.reserve, false),
        AccountMeta::new_readonly(reserve.lending_market, false),
        // Pyth oracle, switchboard oracle and scope oracle are reserve-specific
        // and read from the reserve's own config in newer klend versions.
        // Placeholder: empty optional accounts.
        AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
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
        }
    }

    #[test]
    fn deposit_rejects_zero_amount() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        assert!(matches!(deposit_ix(&user, &reserve, 0), Err(Error::ZeroAmount)));
    }

    #[test]
    fn deposit_returns_three_instructions() {
        let user = Pubkey::new_unique();
        let reserve = dummy_reserve();
        let ixs = deposit_ix(&user, &reserve, 1_000_000).expect("build");
        assert_eq!(ixs.len(), 3, "ATA-create + refresh + deposit");
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
        assert!(matches!(withdraw_ix(&user, &reserve, 0), Err(Error::ZeroAmount)));
    }

    #[test]
    fn obligation_pda_is_deterministic() {
        let user = Pubkey::new_unique();
        let lm = KAMINO_MAIN_MARKET;
        assert_eq!(derive_user_obligation(&user, &lm), derive_user_obligation(&user, &lm));
    }
}
