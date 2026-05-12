use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client_api::{
    config::RpcSimulateTransactionConfig, response::RpcSimulateTransactionResult,
};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    instruction::Instruction,
    message::{v0::Message as V0Message, VersionedMessage},
    signature::{Keypair, Signature, Signer},
    transaction::VersionedTransaction,
};

#[derive(Clone)]
pub struct RpcContext {
    pub client: Arc<RpcClient>,
}

impl RpcContext {
    pub fn new(rpc_url: String, commitment: CommitmentConfig) -> Self {
        Self {
            client: Arc::new(RpcClient::new_with_commitment(rpc_url, commitment)),
        }
    }

    /// Build a `VersionedTransaction` (v0 message), prepend compute budget
    /// instructions, sign, and broadcast. Returns the signature on success.
    ///
    /// Compute budget defaults: caller-supplied per call. Multiply / JLP need
    /// higher CU than a plain supply.
    pub async fn build_sign_send(
        &self,
        ixs: Vec<Instruction>,
        payer: &Keypair,
        cu_limit: u32,
        priority_fee_microlamports: u64,
    ) -> Result<Signature> {
        let tx = self
            .build_signed(ixs, payer, cu_limit, priority_fee_microlamports)
            .await?;
        self.client
            .send_and_confirm_transaction(&tx)
            .await
            .context("send_and_confirm_transaction")
    }

    /// Build + sign + simulate (no broadcast). Returns the simulation result
    /// including program logs and any error. Zero cost — does not consume SOL.
    ///
    /// Use this to verify instruction layouts and reserve metadata before
    /// committing real funds. A simulation that fails with `InsufficientFunds`
    /// or a custom program error means the layout is correct (the program
    /// ran). A simulation that fails with `InvalidAccountData` means the
    /// layout is wrong.
    pub async fn build_sign_simulate(
        &self,
        ixs: Vec<Instruction>,
        payer: &Keypair,
        cu_limit: u32,
        priority_fee_microlamports: u64,
    ) -> Result<RpcSimulateTransactionResult> {
        let tx = self
            .build_signed(ixs, payer, cu_limit, priority_fee_microlamports)
            .await?;
        let cfg = RpcSimulateTransactionConfig {
            sig_verify: false,
            replace_recent_blockhash: true,
            commitment: Some(self.client.commitment()),
            encoding: None,
            accounts: None,
            min_context_slot: None,
            inner_instructions: false,
        };
        let resp = self
            .client
            .simulate_transaction_with_config(&tx, cfg)
            .await
            .context("simulate_transaction")?;
        Ok(resp.value)
    }

    /// Build + sign a v0 transaction. Compute budget instructions are
    /// prepended automatically so the bytes match between simulate and send.
    async fn build_signed(
        &self,
        ixs: Vec<Instruction>,
        payer: &Keypair,
        cu_limit: u32,
        priority_fee_microlamports: u64,
    ) -> Result<VersionedTransaction> {
        if ixs.is_empty() {
            return Err(anyhow!("no instructions to sign"));
        }

        let mut all_ixs = Vec::with_capacity(ixs.len() + 2);
        all_ixs.push(ComputeBudgetInstruction::set_compute_unit_limit(cu_limit));
        if priority_fee_microlamports > 0 {
            all_ixs.push(ComputeBudgetInstruction::set_compute_unit_price(
                priority_fee_microlamports,
            ));
        }
        all_ixs.extend(ixs);

        let recent_blockhash = self
            .client
            .get_latest_blockhash()
            .await
            .context("get_latest_blockhash")?;

        let msg = V0Message::try_compile(
            &payer.pubkey(),
            &all_ixs,
            &[], // no address lookup tables yet
            recent_blockhash,
        )
        .context("compile v0 message")?;

        let tx = VersionedTransaction::try_new(VersionedMessage::V0(msg), &[payer])
            .context("sign versioned transaction")?;
        Ok(tx)
    }
}

/// Classify a simulation result. Returns (was_layout_valid, summary).
///
/// "Layout valid" means klend ran past account validation. We treat
/// `InvalidAccountData`, `IllegalOwner`, and similar account-shape errors as
/// layout failures. `InsufficientFunds`, `Custom(_)` from klend, etc. mean
/// the program ran — the layout was accepted.
pub fn classify_simulation(result: &RpcSimulateTransactionResult) -> (bool, String) {
    use solana_sdk::instruction::InstructionError;
    use solana_sdk::transaction::TransactionError;
    match &result.err {
        None => (true, "ok (no error)".to_string()),
        Some(TransactionError::InstructionError(idx, ie)) => {
            let s = format!("instruction {idx}: {ie:?}");
            let layout_valid = !matches!(
                ie,
                InstructionError::InvalidAccountData
                    | InstructionError::IllegalOwner
                    | InstructionError::AccountNotExecutable
                    | InstructionError::MissingAccount
                    | InstructionError::AccountBorrowFailed
            );
            (layout_valid, s)
        }
        Some(other) => (false, format!("{other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::instruction::InstructionError;
    use solana_sdk::transaction::TransactionError;

    fn sim_with(err: Option<TransactionError>) -> RpcSimulateTransactionResult {
        RpcSimulateTransactionResult {
            err,
            logs: None,
            accounts: None,
            units_consumed: None,
            return_data: None,
            inner_instructions: None,
            replacement_blockhash: None,
        }
    }

    #[test]
    fn classify_no_error_is_valid() {
        let (ok, _) = classify_simulation(&sim_with(None));
        assert!(ok);
    }

    #[test]
    fn classify_invalid_account_data_is_layout_failure() {
        let err = TransactionError::InstructionError(0, InstructionError::InvalidAccountData);
        let (ok, _) = classify_simulation(&sim_with(Some(err)));
        assert!(!ok);
    }

    #[test]
    fn classify_insufficient_funds_means_layout_was_ok() {
        let err = TransactionError::InstructionError(0, InstructionError::InsufficientFunds);
        let (ok, _) = classify_simulation(&sim_with(Some(err)));
        assert!(ok);
    }

    #[test]
    fn classify_custom_program_error_is_layout_ok() {
        let err = TransactionError::InstructionError(0, InstructionError::Custom(42));
        let (ok, _) = classify_simulation(&sim_with(Some(err)));
        assert!(ok);
    }
}
