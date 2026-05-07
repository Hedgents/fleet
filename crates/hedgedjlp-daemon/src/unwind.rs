//! Withdrawal / unwind flow. v0 stub — returns ok=true.
//! M11 lands the real unwind: close perp shorts (request → keeper) →
//! withdraw collateral → swap JLP → USDC.

use anyhow::Result;
use tracing::info;
use zerox1_protocol::fleet::hedgedjlp::{ReportHedgedJlpWithdraw, WithdrawHedgedJlp};
use zerox1_protocol::fleet::ReportHeader;

use crate::dispatch::DispatchCtx;

pub async fn run_or_simulate(
    _ctx: &DispatchCtx,
    payload: &WithdrawHedgedJlp,
    conv: [u8; 16],
) -> Result<ReportHedgedJlpWithdraw> {
    info!(
        jlp_lamports = payload.jlp_lamports,
        ?conv,
        "hedgedjlp unwind M4 stub — pretending unwind succeeded (real ixns land in M11)"
    );
    Ok(ReportHedgedJlpWithdraw {
        header: ReportHeader::ok(conv),
        usdc_returned_lamports: 0,  // M11 will compute real returned amount
        tx_signatures: vec![],
    })
}
