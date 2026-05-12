//! Jupiter Swap aggregator — types for the v1 swap API.
//!
//! Two endpoints we use:
//!   * `GET  /swap/v1/quote`     — fetch a route quote
//!   * `POST /swap/v1/swap`      — convert a quote into a base64 versioned tx
//!
//! Public lite endpoint: <https://lite-api.jup.ag>. The daemon's
//! `handlers/jupiter.rs` does the HTTP I/O and signing.
//!
//! Replaces our Sanctum router integration after Sanctum's swap API went
//! down in early May 2026 (see fleet brain memory). Jupiter routes through
//! Sanctum's S-Pool internally for INF anyway, so economics are similar.

use serde::{Deserialize, Serialize};

pub const JUPITER_LITE_API: &str = "https://lite-api.jup.ag";

/// The full quote response from `GET /swap/v1/quote`. We pass it back to
/// `POST /swap/v1/swap` verbatim, so kept as `serde_json::Value` to avoid
/// pinning every field of Jupiter's evolving schema. The fields we expose
/// downstream are pulled out via the explicit getters below.
pub type SwapQuote = serde_json::Value;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapBuildParams<'a> {
    /// The full quote object returned by `GET /swap/v1/quote`.
    pub quote_response: &'a SwapQuote,
    /// The wallet pubkey that will sign the returned tx.
    pub user_public_key: String,
    /// Wrap/unwrap native SOL automatically when SOL appears in the route.
    /// Default `true` for SOL-side endpoints; the daemon sets this per-call.
    pub wrap_and_unwrap_sol: bool,
}

/// Response from `POST /swap/v1/swap`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapBuildResp {
    /// Base64-encoded `VersionedTransaction` ready for signing + broadcast.
    pub swap_transaction: String,
    pub last_valid_block_height: u64,
    #[serde(default)]
    pub prioritization_fee_lamports: u64,
    #[serde(default)]
    pub compute_unit_limit: u32,
}

/// Convenience: pull the (in_amount, out_amount, route step count) from a
/// quote so the daemon can summarize without parsing the whole object.
pub fn quote_summary(q: &SwapQuote) -> (Option<u64>, Option<u64>, usize) {
    let in_amount = q
        .get("inAmount")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok());
    let out_amount = q
        .get("outAmount")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok());
    let route_steps = q
        .get("routePlan")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    (in_amount, out_amount, route_steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_summary_extracts_amounts_and_steps() {
        let q: SwapQuote = serde_json::json!({
            "inAmount": "1000000000",
            "outAmount": "783850491",
            "routePlan": [
                {"swapInfo": {"label": "Manifest"}},
                {"swapInfo": {"label": "Meteora DLMM"}}
            ]
        });
        let (in_amt, out_amt, steps) = quote_summary(&q);
        assert_eq!(in_amt, Some(1_000_000_000));
        assert_eq!(out_amt, Some(783_850_491));
        assert_eq!(steps, 2);
    }

    #[test]
    fn quote_summary_handles_missing_fields() {
        let q: SwapQuote = serde_json::json!({});
        let (in_amt, out_amt, steps) = quote_summary(&q);
        assert!(in_amt.is_none());
        assert!(out_amt.is_none());
        assert_eq!(steps, 0);
    }

    #[test]
    fn swap_build_resp_parses_documented_example() {
        let json = r#"{
            "swapTransaction": "AQABAg==",
            "lastValidBlockHeight": 395653434,
            "prioritizationFeeLamports": 0,
            "computeUnitLimit": 1400000
        }"#;
        let r: SwapBuildResp = serde_json::from_str(json).expect("parse");
        assert_eq!(r.swap_transaction, "AQABAg==");
        assert_eq!(r.last_valid_block_height, 395653434);
        assert_eq!(r.compute_unit_limit, 1_400_000);
    }
}
