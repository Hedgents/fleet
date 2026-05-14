use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client_api::{
    config::RpcSimulateTransactionConfig, response::RpcSimulateTransactionResult,
};
use solana_sdk::{
    address_lookup_table::{state::AddressLookupTable, AddressLookupTableAccount},
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    instruction::Instruction,
    message::{v0::Message as V0Message, VersionedMessage},
    pubkey::Pubkey,
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
        self.build_sign_send_with_alts(ixs, payer, cu_limit, priority_fee_microlamports, &[])
            .await
    }

    /// `build_sign_send` with on-chain Address Lookup Tables. Each entry in
    /// `alt_addresses` is fetched and supplied to the v0 message compiler;
    /// accounts present in any of the tables become 1-byte indexed references
    /// instead of 32-byte inline pubkeys. Used by multiply's lever-up bundle
    /// to keep the encoded tx under the 1232-byte raw limit.
    pub async fn build_sign_send_with_alts(
        &self,
        ixs: Vec<Instruction>,
        payer: &Keypair,
        cu_limit: u32,
        priority_fee_microlamports: u64,
        alt_addresses: &[Pubkey],
    ) -> Result<Signature> {
        let tx = self
            .build_signed(
                ixs,
                payer,
                cu_limit,
                priority_fee_microlamports,
                alt_addresses,
            )
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
        self.build_sign_simulate_with_alts(ixs, payer, cu_limit, priority_fee_microlamports, &[])
            .await
    }

    /// `build_sign_simulate` with Address Lookup Tables (see
    /// `build_sign_send_with_alts`).
    pub async fn build_sign_simulate_with_alts(
        &self,
        ixs: Vec<Instruction>,
        payer: &Keypair,
        cu_limit: u32,
        priority_fee_microlamports: u64,
        alt_addresses: &[Pubkey],
    ) -> Result<RpcSimulateTransactionResult> {
        let tx = self
            .build_signed(
                ixs,
                payer,
                cu_limit,
                priority_fee_microlamports,
                alt_addresses,
            )
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
    ///
    /// `alt_addresses` is the list of on-chain Address Lookup Tables to feed
    /// the v0 message compiler. Empty by default — preserves the behaviour
    /// for callers (stable-yield, hedgedjlp, etc.) that don't need ALTs.
    /// Multiply uses Kamino's main-market ALT to keep the lever-up bundle
    /// under the 1232-byte raw tx limit.
    async fn build_signed(
        &self,
        ixs: Vec<Instruction>,
        payer: &Keypair,
        cu_limit: u32,
        priority_fee_microlamports: u64,
        alt_addresses: &[Pubkey],
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

        let alt_accounts = self.fetch_lookup_tables(alt_addresses).await?;

        let recent_blockhash = self
            .client
            .get_latest_blockhash()
            .await
            .context("get_latest_blockhash")?;

        let msg =
            V0Message::try_compile(&payer.pubkey(), &all_ixs, &alt_accounts, recent_blockhash)
                .context("compile v0 message")?;

        let tx = VersionedTransaction::try_new(VersionedMessage::V0(msg), &[payer])
            .context("sign versioned transaction")?;
        Ok(tx)
    }

    /// Fetch and decode each Address Lookup Table by address. Returns an
    /// empty vec when `addrs` is empty (no network round-trip). Each ALT
    /// account is decoded into the addresses it contains; the resulting
    /// `AddressLookupTableAccount` values are fed straight to
    /// `Message::try_compile`.
    async fn fetch_lookup_tables(
        &self,
        addrs: &[Pubkey],
    ) -> Result<Vec<AddressLookupTableAccount>> {
        if addrs.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(addrs.len());
        for key in addrs {
            let acct = self
                .client
                .get_account(key)
                .await
                .with_context(|| format!("fetch ALT account {key}"))?;
            let table = AddressLookupTable::deserialize(&acct.data)
                .map_err(|e| anyhow!("deserialize ALT {key}: {e:?}"))?;
            out.push(AddressLookupTableAccount {
                key: *key,
                addresses: table.addresses.to_vec(),
            });
        }
        Ok(out)
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
    use solana_sdk::hash::Hash;
    use solana_sdk::instruction::{AccountMeta, InstructionError};
    use solana_sdk::transaction::TransactionError;

    /// Pure compile helper for unit-testing the ALT shrink. Mirrors what
    /// `build_signed` calls into after fetching the lookup tables, minus the
    /// RPC round-trip / signing.
    fn compile_v0_unsigned(
        payer: &Pubkey,
        ixs: &[Instruction],
        alts: &[AddressLookupTableAccount],
    ) -> V0Message {
        V0Message::try_compile(payer, ixs, alts, Hash::new_unique()).expect("compile v0")
    }

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

    /// v0.1.20: lever-up shrink. A non-empty ALT covering the static
    /// (read-only, non-signer) accounts in an ixn list must produce a
    /// strictly smaller serialised v0 message than the same ixns compiled
    /// without a lookup table — each ALT-covered account drops from a
    /// 32-byte inline pubkey to a 1-byte index. This is the primitive
    /// multiply's lever-up bundle relies on to clear the 1232-byte raw
    /// limit.
    #[test]
    fn build_signed_with_lookup_tables_compiles_smaller_tx() {
        let payer = Pubkey::new_unique();
        let program = Pubkey::new_unique();
        // 12 read-only accounts referenced by a dummy ixn list — large
        // enough that ALT-indexing 8 of them clearly beats inline bytes.
        let accounts: Vec<Pubkey> = (0..12).map(|_| Pubkey::new_unique()).collect();
        let metas: Vec<AccountMeta> = accounts
            .iter()
            .map(|k| AccountMeta::new_readonly(*k, false))
            .collect();
        let ix = Instruction {
            program_id: program,
            accounts: metas,
            data: vec![0u8; 8],
        };
        let ixs = vec![ix];

        let baseline = compile_v0_unsigned(&payer, &ixs, &[]);
        let baseline_bytes = bincode::serialize(&baseline).unwrap();

        // ALT covering 8 of the 12 accounts.
        let alt = AddressLookupTableAccount {
            key: Pubkey::new_unique(),
            addresses: accounts.iter().take(8).copied().collect(),
        };
        let with_alt = compile_v0_unsigned(&payer, &ixs, std::slice::from_ref(&alt));
        let with_alt_bytes = bincode::serialize(&with_alt).unwrap();

        assert!(
            with_alt_bytes.len() < baseline_bytes.len(),
            "ALT-compiled v0 message must be strictly smaller: with_alt={} bytes, baseline={} bytes",
            with_alt_bytes.len(),
            baseline_bytes.len()
        );
    }
}
