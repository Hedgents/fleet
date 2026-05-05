//! Leverage loop. Placeholder body for M4 — M6 implements the real
//! supply→borrow→swap rounds. For M4, sim-only returns Ok with
//! resulting_ltv_bps=0; submit-mode returns Err to make the failure
//! mode loud during the M4-only review.

use anyhow::{anyhow, Result};
use tracing::info;
use zerox1_protocol::fleet::multiply::{AssignMultiply, ReportMultiply};
use zerox1_protocol::fleet::ReportHeader;

use crate::dispatch::DispatchCtx;

/// Either simulate the leverage entry or actually submit it (per
/// `ctx.simulate_only`). M6 replaces this with the real multi-round loop.
pub async fn run_or_simulate(
    ctx: &DispatchCtx,
    assign: &AssignMultiply,
    conv: [u8; 16],
) -> Result<ReportMultiply> {
    info!(
        simulate_only = ctx.simulate_only,
        target_ltv_bps = assign.target_ltv_bps,
        "leverage::run_or_simulate (M4 placeholder — M6 implements)"
    );
    if ctx.simulate_only {
        Ok(ReportMultiply {
            header: ReportHeader::ok(conv),
            resulting_ltv_bps: 0,
            tx_signature: None,
        })
    } else {
        Err(anyhow!("leverage loop not yet implemented (M6)"))
    }
}
