//! Kamino USDC supply execution. M4 stub — returns a successful Report
//! without doing any chain work. M6 replaces this with the real
//! lendingMarketDeposit ixn + sign + submit/simulate path.

use anyhow::Result;
use tracing::info;
use zerox1_protocol::fleet::stable_lend::{AssignStableLend, ReportStableLend};
use zerox1_protocol::fleet::ReportHeader;

use crate::dispatch::DispatchCtx;

/// M4 stub. Returns ok=true with deposited=requested + tx_signature=None.
/// M6 will replace the body with the real Kamino deposit flow.
pub async fn run_or_simulate(
    _ctx: &DispatchCtx,
    payload: &AssignStableLend,
    conv: [u8; 16],
) -> Result<ReportStableLend> {
    info!(
        amount = payload.usdc_lamports,
        ?conv,
        "stable-yield M4 stub — pretending deposit succeeded (real ixn lands in M6)"
    );
    Ok(ReportStableLend {
        header: ReportHeader::ok(conv),
        deposited_usdc_lamports: payload.usdc_lamports,
        current_apr_bps: 0,
        tx_signature: None,
    })
}
