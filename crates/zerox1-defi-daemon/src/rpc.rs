use std::future::Future;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client_api::{
    config::RpcSimulateTransactionConfig, response::RpcSimulateTransactionResult,
};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    hash::Hash,
    instruction::Instruction,
    message::{v0::Message as V0Message, VersionedMessage},
    signature::{Keypair, Signature, Signer},
    transaction::VersionedTransaction,
};

/// RPC context with failover support. The primary `client` is exposed for
/// low-criticality reads (loaders run at startup, position decoders for
/// Risk Watcher); state-changing or latency-sensitive paths
/// (broadcast, simulate, blockhash refresh) use `try_with_failover` to
/// transparently retry against `fallbacks` on transient errors.
#[derive(Clone)]
pub struct RpcContext {
    /// Primary RPC client.
    pub client: Arc<RpcClient>,
    /// Ordered fallback RPC clients. Empty if the daemon was started with
    /// only one `--rpc-url`.
    pub fallbacks: Vec<Arc<RpcClient>>,
}

impl RpcContext {
    /// Construct a single-RPC context with no fallbacks. Convenience wrapper
    /// equivalent to `with_fallbacks(rpc_url, vec![], commitment)`.
    #[allow(dead_code)]
    pub fn new(rpc_url: String, commitment: CommitmentConfig) -> Self {
        Self {
            client: Arc::new(RpcClient::new_with_commitment(rpc_url, commitment)),
            fallbacks: Vec::new(),
        }
    }

    pub fn with_fallbacks(
        rpc_url: String,
        fallback_urls: Vec<String>,
        commitment: CommitmentConfig,
    ) -> Self {
        Self {
            client: Arc::new(RpcClient::new_with_commitment(rpc_url, commitment)),
            fallbacks: fallback_urls
                .into_iter()
                .map(|u| Arc::new(RpcClient::new_with_commitment(u, commitment)))
                .collect(),
        }
    }

    /// Run an async closure against the primary client; on error, retry
    /// against each fallback in order. Returns the first successful result
    /// or the last error if all clients fail.
    ///
    /// The closure receives a borrowed `&RpcClient` so it must be cheap to
    /// construct (it'll be invoked once per RPC). Logs each fallback attempt
    /// at warn-level so operators can see when the primary is degraded.
    async fn try_with_failover<F, Fut, T>(&self, label: &'static str, mut f: F) -> Result<T>
    where
        F: FnMut(Arc<RpcClient>) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        // First attempt: primary.
        let primary_err = match f(self.client.clone()).await {
            Ok(v) => return Ok(v),
            Err(e) => e,
        };
        if self.fallbacks.is_empty() {
            return Err(primary_err.context(format!("rpc {label} (no fallbacks configured)")));
        }
        tracing::warn!(error = %primary_err, "primary RPC failed for {label}; trying {} fallbacks", self.fallbacks.len());

        let mut last_err = primary_err;
        for (i, fb) in self.fallbacks.iter().enumerate() {
            match f(fb.clone()).await {
                Ok(v) => {
                    tracing::warn!("fallback RPC #{i} succeeded for {label}");
                    return Ok(v);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "fallback RPC #{i} also failed for {label}");
                    last_err = e;
                }
            }
        }
        Err(last_err.context(format!(
            "rpc {label} (all {} fallbacks exhausted)",
            self.fallbacks.len()
        )))
    }

    /// Failover-aware blockhash fetch.
    pub async fn get_latest_blockhash_with_failover(&self) -> Result<Hash> {
        self.try_with_failover("get_latest_blockhash", |c| async move {
            c.get_latest_blockhash()
                .await
                .context("get_latest_blockhash")
        })
        .await
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
        let tx = Arc::new(tx);
        self.try_with_failover("send_and_confirm_transaction", move |c| {
            let tx = tx.clone();
            async move {
                c.send_and_confirm_transaction(tx.as_ref())
                    .await
                    .context("send_and_confirm_transaction")
            }
        })
        .await
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
        self.simulate_with_failover(tx).await
    }

    async fn simulate_with_failover(
        &self,
        tx: VersionedTransaction,
    ) -> Result<RpcSimulateTransactionResult> {
        let tx = Arc::new(tx);
        let commitment = self.client.commitment();
        self.try_with_failover("simulate_transaction", move |c| {
            let tx = tx.clone();
            async move {
                let cfg = RpcSimulateTransactionConfig {
                    sig_verify: false,
                    replace_recent_blockhash: true,
                    commitment: Some(commitment),
                    encoding: None,
                    accounts: None,
                    min_context_slot: None,
                    inner_instructions: false,
                };
                let resp = c
                    .simulate_transaction_with_config(tx.as_ref(), cfg)
                    .await
                    .context("simulate_transaction")?;
                Ok(resp.value)
            }
        })
        .await
    }

    /// Sign a pre-built `VersionedTransaction` (e.g. one returned by the
    /// Sanctum router) and broadcast. Refreshes the recent_blockhash to the
    /// current one before signing — Sanctum embeds a blockhash that may have
    /// already expired by the time we receive it.
    pub async fn sign_existing_send(
        &self,
        mut tx: VersionedTransaction,
        payer: &Keypair,
    ) -> Result<Signature> {
        self.refresh_blockhash_and_sign(&mut tx, payer).await?;
        let tx = Arc::new(tx);
        self.try_with_failover("send_and_confirm_transaction", move |c| {
            let tx = tx.clone();
            async move {
                c.send_and_confirm_transaction(tx.as_ref())
                    .await
                    .context("send_and_confirm_transaction")
            }
        })
        .await
    }

    /// Sign a pre-built `VersionedTransaction` and simulate it. Same blockhash
    /// refresh logic as `sign_existing_send`.
    pub async fn sign_existing_simulate(
        &self,
        mut tx: VersionedTransaction,
        payer: &Keypair,
    ) -> Result<RpcSimulateTransactionResult> {
        self.refresh_blockhash_and_sign(&mut tx, payer).await?;
        self.simulate_with_failover(tx).await
    }

    async fn refresh_blockhash_and_sign(
        &self,
        tx: &mut VersionedTransaction,
        payer: &Keypair,
    ) -> Result<()> {
        let recent_blockhash = self.get_latest_blockhash_with_failover().await?;
        tx.message.set_recent_blockhash(recent_blockhash);
        // Re-sign — payer signature is at slot 0 (Sanctum builds with payer
        // as first signer).
        let new_sig = payer.sign_message(&tx.message.serialize());
        if tx.signatures.is_empty() {
            tx.signatures.push(new_sig);
        } else {
            tx.signatures[0] = new_sig;
        }
        Ok(())
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

        let recent_blockhash = self.get_latest_blockhash_with_failover().await?;

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
///
/// `AccountNotFound` at the transaction level means the fee payer doesn't
/// exist on-chain (never funded). This is an infrastructure issue unrelated
/// to the klend account layout; we report it as a separate state.
pub fn classify_simulation(result: &RpcSimulateTransactionResult) -> (bool, String) {
    use solana_sdk::instruction::InstructionError;
    use solana_sdk::transaction::TransactionError;
    match &result.err {
        None => (true, "ok (no error)".to_string()),
        Some(TransactionError::AccountNotFound) => {
            // Fee payer or a lookup-table entry doesn't exist on this cluster.
            // Not a klend layout error — fund the fee payer and retry.
            (
                true,
                "AccountNotFound: fee payer not funded on this cluster".to_string(),
            )
        }
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
    fn classify_account_not_found_is_fee_payer_issue_not_layout_failure() {
        let (ok, summary) = classify_simulation(&sim_with(Some(TransactionError::AccountNotFound)));
        assert!(
            ok,
            "AccountNotFound is a wallet-funding issue, not a layout error"
        );
        assert!(
            summary.contains("fee payer"),
            "summary should explain the cause"
        );
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

    // ── failover unit tests ────────────────────────────────────────────────
    //
    // We can't easily mock RpcClient (it's not trait-based) so these tests
    // exercise the failover policy via a stand-alone helper that mirrors
    // `try_with_failover`'s structure.

    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn try_failover_for_test<F, Fut, T>(
        attempts: &AtomicUsize,
        primary_should_fail: bool,
        fallback_count: usize,
        fallback_succeeds_at: Option<usize>,
        mut f: F,
    ) -> std::result::Result<T, &'static str>
    where
        F: FnMut(usize) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<T, &'static str>>,
    {
        // Mirror of try_with_failover's policy.
        attempts.fetch_add(1, Ordering::SeqCst);
        let primary = f(0).await;
        if primary.is_ok() && !primary_should_fail {
            return primary;
        }
        for i in 0..fallback_count {
            attempts.fetch_add(1, Ordering::SeqCst);
            let res = f(i + 1).await;
            if let Ok(v) = res {
                if Some(i) == fallback_succeeds_at {
                    return Ok(v);
                }
            }
        }
        Err("all fallbacks failed")
    }

    #[tokio::test]
    async fn failover_uses_primary_when_it_succeeds() {
        let attempts = AtomicUsize::new(0);
        let result = try_failover_for_test(&attempts, false, 2, Some(0), |_idx| async {
            Ok::<_, &'static str>("primary value")
        })
        .await;
        assert_eq!(result, Ok("primary value"));
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "should not call fallbacks on primary success"
        );
    }

    #[tokio::test]
    async fn failover_falls_back_when_primary_fails() {
        let attempts = AtomicUsize::new(0);
        let result = try_failover_for_test(&attempts, true, 2, Some(0), |idx| async move {
            if idx == 0 {
                Err::<&'static str, _>("primary error")
            } else if idx == 1 {
                Ok::<_, &'static str>("fallback value")
            } else {
                Err("unused")
            }
        })
        .await;
        assert_eq!(result, Ok("fallback value"));
        // 1 primary + 1 fallback attempt
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn failover_returns_error_when_all_fail() {
        let attempts = AtomicUsize::new(0);
        let result: std::result::Result<&'static str, _> =
            try_failover_for_test(&attempts, true, 3, None, |_idx| async {
                Err::<&'static str, _>("nope")
            })
            .await;
        assert!(result.is_err());
        // 1 primary + 3 fallbacks
        assert_eq!(attempts.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn rpc_context_with_fallbacks_constructs_correctly() {
        let ctx = RpcContext::with_fallbacks(
            "https://primary.example".to_string(),
            vec![
                "https://fb1.example".to_string(),
                "https://fb2.example".to_string(),
            ],
            CommitmentConfig::confirmed(),
        );
        assert_eq!(ctx.fallbacks.len(), 2);
    }

    #[test]
    fn rpc_context_new_has_no_fallbacks() {
        let ctx = RpcContext::new(
            "https://primary.example".to_string(),
            CommitmentConfig::confirmed(),
        );
        assert!(ctx.fallbacks.is_empty());
    }
}
