//! Jito SPL Stake Pool — `DepositSol` instruction builder.
//!
//! This is the direct path SOL → jitoSOL: zero swap spread, no API
//! dependency, smaller transaction footprint than going through Jupiter.
//! The Multiply Agent uses this for the swap step in atomic leveraged
//! deposits.
//!
//! ## DepositSol layout (verified against on-chain tx on 2026-05-04)
//!
//! ```text
//! variant byte: 0x0e
//! args:         lamports (u64 LE) = 9 bytes total
//!
//! accounts (10):
//! [0] stake_pool                       (writable)   the Jito pool
//! [1] withdraw_authority               (readonly)   PDA([pool, "withdraw"])
//! [2] reserve_stake                    (writable)   pool's reserve stake account
//! [3] user_lamports_source             (writable, signer)  user paying SOL
//! [4] user_pool_token_destination      (writable)   user's jitoSOL ATA
//! [5] manager_fee_account              (writable)   pool's fee receiver
//! [6] referrer_pool_tokens_account     (writable)   referral receiver
//!                                                    (pass user's jitoSOL ATA = self)
//! [7] pool_mint                        (writable)   jitoSOL mint
//! [8] system_program                   (readonly)
//! [9] token_program                    (readonly)
//! ```

use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    system_program,
};
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;

use crate::{
    constants::{JITOSOL_MINT, JITO_STAKE_POOL, SPL_STAKE_POOL_PROGRAM_ID, TOKEN_PROGRAM_ID},
    util::ata,
    Error, Result,
};

/// `DepositSol` instruction variant in the SPL stake-pool program.
const DEPOSIT_SOL_VARIANT: u8 = 0x0e;

/// Subset of the Jito stake-pool account fields needed to build a
/// `DepositSol` instruction. Loaded once per dispatch by the daemon —
/// the exchange rate (`total_lamports` / `pool_token_supply`) is needed
/// to compute the jitoSOL amount the user will receive for a given SOL
/// stake.
#[derive(Debug, Clone)]
pub struct StakePoolMeta {
    pub stake_pool: Pubkey,
    pub withdraw_authority: Pubkey,
    pub reserve_stake: Pubkey,
    pub manager_fee_account: Pubkey,
    pub pool_mint: Pubkey,
    /// Pool's total active+reserve SOL lamports under management. Together
    /// with `pool_token_supply`, defines the SOL→jitoSOL exchange rate.
    /// Read from offset 258 of the StakePool account.
    pub total_lamports: u64,
    /// Total jitoSOL supply outstanding (in 9-decimal jitoSOL lamports).
    /// Read from offset 266 of the StakePool account.
    pub pool_token_supply: u64,
}

impl StakePoolMeta {
    /// Convenience constructor for the default Jito stake pool with all
    /// fields filled in by the caller from on-chain decode.
    pub fn jito(
        withdraw_authority: Pubkey,
        reserve_stake: Pubkey,
        manager_fee_account: Pubkey,
    ) -> Self {
        Self {
            stake_pool: JITO_STAKE_POOL,
            withdraw_authority,
            reserve_stake,
            manager_fee_account,
            pool_mint: JITOSOL_MINT,
            // Use 1:1 default for unit tests that don't care about the rate.
            // load_jito_pool overrides these from on-chain data in production.
            total_lamports: 1,
            pool_token_supply: 1,
        }
    }

    /// Convert a SOL-lamports stake amount to the jitoSOL-lamports the
    /// user will receive, using the pool's current exchange rate. Returns
    /// the floor of `stake_lamports * pool_token_supply / total_lamports`,
    /// which matches the Jito stake pool's own integer arithmetic.
    ///
    /// v0.1.13 fix: callers previously assumed 1:1 SOL:jitoSOL with a
    /// 0.5% haircut. With 1 jitoSOL ≈ 1.278 SOL on mainnet, that estimate
    /// was ~27% too high and Kamino's deposit step failed with the SPL
    /// Token `0x1 = InsufficientFunds` because the user's jitoSOL ATA
    /// held less than the bundle was trying to transfer.
    pub fn sol_to_jitosol_lamports(&self, stake_lamports: u64) -> u64 {
        if self.total_lamports == 0 {
            return 0;
        }
        ((stake_lamports as u128) * (self.pool_token_supply as u128)
            / (self.total_lamports as u128)) as u64
    }
}

/// Derive the SPL stake-pool withdraw authority PDA for a given stake pool.
/// Seeds: `[stake_pool, "withdraw"]`.
pub fn derive_withdraw_authority(stake_pool: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[stake_pool.as_ref(), b"withdraw"],
        &SPL_STAKE_POOL_PROGRAM_ID,
    )
    .0
}

/// Build the instruction sequence to deposit `lamports` of SOL into the
/// Jito stake pool, minting the equivalent jitoSOL to the user.
///
/// Returns:
/// 1. Idempotent ATA-create for the user's jitoSOL ATA
/// 2. The `DepositSol` instruction
///
/// At current rates (1 jitoSOL = 1.277 SOL), depositing 1 SOL yields
/// ~0.7838 jitoSOL minus a small pool fee.
pub fn deposit_sol_ix(
    user: &Pubkey,
    pool: &StakePoolMeta,
    lamports: u64,
) -> Result<Vec<Instruction>> {
    if lamports == 0 {
        return Err(Error::ZeroAmount);
    }
    let user_jitosol_ata = ata(user, &pool.pool_mint);

    let mut ixs = Vec::with_capacity(2);

    ixs.push(create_associated_token_account_idempotent(
        user,
        user,
        &pool.pool_mint,
        &TOKEN_PROGRAM_ID,
    ));

    let mut data = Vec::with_capacity(9);
    data.push(DEPOSIT_SOL_VARIANT);
    data.extend_from_slice(&lamports.to_le_bytes());

    let accounts = vec![
        AccountMeta::new(pool.stake_pool, false), // [0] stake_pool (w)
        AccountMeta::new_readonly(pool.withdraw_authority, false), // [1] withdraw_authority
        AccountMeta::new(pool.reserve_stake, false), // [2] reserve_stake (w)
        AccountMeta::new(*user, true),            // [3] user_lamports_source (w, signer)
        AccountMeta::new(user_jitosol_ata, false), // [4] user_pool_token_destination (w)
        AccountMeta::new(pool.manager_fee_account, false), // [5] manager_fee_account (w)
        AccountMeta::new(user_jitosol_ata, false), // [6] referrer_pool_tokens_account = self (w)
        AccountMeta::new(pool.pool_mint, false),  // [7] pool_mint (w)
        AccountMeta::new_readonly(system_program::ID, false), // [8] system_program
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false), // [9] token_program
    ];

    ixs.push(Instruction {
        program_id: SPL_STAKE_POOL_PROGRAM_ID,
        accounts,
        data,
    });

    Ok(ixs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn dummy_pool() -> StakePoolMeta {
        StakePoolMeta::jito(
            derive_withdraw_authority(&JITO_STAKE_POOL),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        )
    }

    #[test]
    fn withdraw_authority_matches_known_jito_address() {
        // Verified against on-chain DepositSol tx on 2026-05-04.
        let expected = Pubkey::from_str("6iQKfEyhr3bZMotVkW6beNZz5CPAkiwvgV2CTje9pVSS").unwrap();
        assert_eq!(derive_withdraw_authority(&JITO_STAKE_POOL), expected);
    }

    #[test]
    fn deposit_sol_rejects_zero() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        assert!(matches!(
            deposit_sol_ix(&user, &pool, 0),
            Err(Error::ZeroAmount)
        ));
    }

    #[test]
    fn deposit_sol_returns_two_instructions() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let ixs = deposit_sol_ix(&user, &pool, 1_000_000_000).expect("build");
        assert_eq!(ixs.len(), 2, "ATA-create + DepositSol");
        assert_eq!(ixs[1].program_id, SPL_STAKE_POOL_PROGRAM_ID);
    }

    #[test]
    fn deposit_sol_has_10_accounts_in_correct_order() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let ixs = deposit_sol_ix(&user, &pool, 1_000_000_000).expect("build");
        let ix = ixs.last().unwrap();
        assert_eq!(ix.accounts.len(), 10);
        assert_eq!(ix.accounts[0].pubkey, pool.stake_pool);
        assert_eq!(ix.accounts[1].pubkey, pool.withdraw_authority);
        assert_eq!(ix.accounts[2].pubkey, pool.reserve_stake);
        assert_eq!(ix.accounts[3].pubkey, user);
        assert!(ix.accounts[3].is_signer);
        // accounts[4] and [6] are both the user's jitoSOL ATA (self-referral)
        assert_eq!(ix.accounts[4].pubkey, ix.accounts[6].pubkey);
        assert_eq!(ix.accounts[5].pubkey, pool.manager_fee_account);
        assert_eq!(ix.accounts[7].pubkey, pool.pool_mint);
        assert_eq!(ix.accounts[8].pubkey, system_program::ID);
        assert_eq!(ix.accounts[9].pubkey, TOKEN_PROGRAM_ID);
    }

    #[test]
    fn sol_to_jitosol_uses_pool_exchange_rate() {
        // Mainnet on 2026-05-13: 1 jitoSOL ≈ 1.279 SOL.
        let mut pool = dummy_pool();
        pool.total_lamports = 9_860_677_886_811_084;
        pool.pool_token_supply = 7_709_932_497_630_153;
        // 50M SOL lamports should map to ≈ 39.1M jitoSOL lamports, not 50M.
        let got = pool.sol_to_jitosol_lamports(50_000_000);
        assert!(
            (39_000_000..=39_200_000).contains(&got),
            "expected ~39.1M jitoSOL, got {got}"
        );
    }

    #[test]
    fn sol_to_jitosol_one_to_one_when_supply_equals_lamports() {
        let mut pool = dummy_pool();
        pool.total_lamports = 1_000_000_000;
        pool.pool_token_supply = 1_000_000_000;
        assert_eq!(pool.sol_to_jitosol_lamports(123_456_789), 123_456_789);
    }

    #[test]
    fn sol_to_jitosol_zero_total_returns_zero() {
        let mut pool = dummy_pool();
        pool.total_lamports = 0;
        pool.pool_token_supply = 1;
        assert_eq!(pool.sol_to_jitosol_lamports(1_000_000), 0);
    }

    #[test]
    fn deposit_sol_data_starts_with_variant_byte() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let ixs = deposit_sol_ix(&user, &pool, 1_234_567).expect("build");
        let ix = ixs.last().unwrap();
        assert_eq!(ix.data.len(), 9, "1 variant + 8 lamports = 9 bytes");
        assert_eq!(ix.data[0], DEPOSIT_SOL_VARIANT);
        let lamports = u64::from_le_bytes(ix.data[1..9].try_into().unwrap());
        assert_eq!(lamports, 1_234_567);
    }
}
