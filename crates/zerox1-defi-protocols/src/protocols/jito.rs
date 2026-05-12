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
/// `DepositSol` instruction. Loaded once at startup by the daemon.
#[derive(Debug, Clone)]
pub struct StakePoolMeta {
    pub stake_pool: Pubkey,
    pub withdraw_authority: Pubkey,
    pub reserve_stake: Pubkey,
    pub manager_fee_account: Pubkey,
    pub pool_mint: Pubkey,
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
        }
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
