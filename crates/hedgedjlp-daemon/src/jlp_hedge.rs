//! Combined JLP-buy + hedge-leg execution. v0 stub — returns
//! ok=true Report without chain work. M6 lands JLP buy via Jupiter
//! swap; M8 lands Jupiter Perps hedge-leg open via 2-tx request flow.

use anyhow::Result;
use tracing::info;
use zerox1_protocol::fleet::hedgedjlp::{AssignHedgedJlp, ReportHedgedJlp};
use zerox1_protocol::fleet::ReportHeader;

use crate::dispatch::DispatchCtx;

pub async fn run_or_simulate(
    _ctx: &DispatchCtx,
    payload: &AssignHedgedJlp,
    conv: [u8; 16],
) -> Result<ReportHedgedJlp> {
    info!(
        usdc_lamports = payload.usdc_lamports,
        target_delta_bps = payload.target_delta_bps,
        max_borrow_rate_bps = payload.max_borrow_rate_bps,
        ?conv,
        "hedgedjlp M4 stub — pretending JLP+hedge succeeded (real ixns land in M6+M8)"
    );
    Ok(ReportHedgedJlp {
        header: ReportHeader::ok(conv),
        jlp_acquired_lamports: payload.usdc_lamports / 2,  // synthetic 50/50 split
        hedge_notional_usdc: payload.usdc_lamports / 2,
        current_delta_bps: 0,
        tx_signatures: vec![],
    })
}
