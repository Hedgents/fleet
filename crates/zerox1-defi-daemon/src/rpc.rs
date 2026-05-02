use std::sync::Arc;

use anyhow::{Context, Result};
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::Instruction,
    signature::{Keypair, Signature, Signer},
    transaction::Transaction,
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

    /// Build a legacy `Transaction` from instructions, sign with `payer`, broadcast.
    /// Returns the transaction signature on success.
    ///
    /// NOTE: For Kamino + multi-instruction Anchor flows we will switch to
    /// `VersionedTransaction` + Address Lookup Tables in a follow-up. Legacy is
    /// fine for the USDC supply/withdraw scaffold (single ATA + refresh + ix).
    pub async fn build_sign_send(
        &self,
        ixs: Vec<Instruction>,
        payer: &Keypair,
    ) -> Result<Signature> {
        let recent_blockhash = self
            .client
            .get_latest_blockhash()
            .await
            .context("get_latest_blockhash")?;

        let mut tx = Transaction::new_with_payer(&ixs, Some(&payer.pubkey()));
        tx.sign(&[payer], recent_blockhash);

        self.client
            .send_and_confirm_transaction(&tx)
            .await
            .context("send_and_confirm_transaction")
    }
}
