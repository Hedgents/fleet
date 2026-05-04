//! Sanctum router — types and constants for the HTTP swap API.
//!
//! Unlike Kamino/Pyth, Sanctum builds the versioned transaction server-side.
//! This module only exposes:
//!   - The router base URL
//!   - Wire types matching the OpenAPI spec at https://sanctum-s-api.fly.dev/doc
//!
//! The daemon's `handlers/sanctum.rs` does the HTTP I/O and signing.

use serde::{Deserialize, Serialize};

/// Base URL for the Sanctum S router. Holds the `/v1/swap/quote`,
/// `/v1/swap`, `/v1/liquidity/*` endpoints.
pub const SANCTUM_ROUTER_URL: &str = "https://sanctum-s-api.fly.dev";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SwapMode {
    ExactIn,
    ExactOut,
}

/// Which source venue Sanctum's router chose for the swap.
/// `SPool` = the multi-LST Sanctum Infinity pool. `Stakedex` = direct
/// stake-pool deposit/withdraw. `Jup` = Jupiter aggregator fallback.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SwapSrc {
    SPool,
    Stakedex,
    Jup,
}

/// Response from `GET /v1/swap/quote`.
///
/// Amounts are stringified u64 (`U64Str` in the OpenAPI). We parse them via
/// serde to plain `u64` strings here — the daemon converts to numeric on
/// display.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapQuoteResp {
    pub in_amount: String,
    pub out_amount: String,
    pub fee_amount: String,
    pub fee_mint: String,
    pub fee_pct: String,
    pub swap_src: SwapSrc,
}

/// Body for `POST /v1/swap`.
///
/// `signer` is the wallet pubkey that will sign the returned tx — Sanctum
/// embeds it as the fee payer in the message it builds.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapParams {
    pub input: String,
    pub output_lst_mint: String,
    pub amount: String,
    pub mode: SwapMode,
    pub signer: String,
    pub swap_src: SwapSrc,
}

/// Response from `POST /v1/swap`. `tx` is a base64-encoded VersionedTransaction
/// with the fee payer signature slot left empty for the client to fill.
#[derive(Debug, Clone, Deserialize)]
pub struct SwapResp {
    pub tx: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_resp_parses_documented_example() {
        let json = r#"{
            "feeAmount": "92000",
            "feeMint": "So11111111111111111111111111111111111111112",
            "feePct": "0.0001",
            "inAmount": "1000000000",
            "outAmount": "919908000",
            "swapSrc": "SPool"
        }"#;
        let q: SwapQuoteResp = serde_json::from_str(json).expect("parse");
        assert_eq!(q.in_amount, "1000000000");
        assert_eq!(q.out_amount, "919908000");
        assert_eq!(q.swap_src, SwapSrc::SPool);
    }

    #[test]
    fn swap_params_serializes_camel_case() {
        let p = SwapParams {
            input: "So11111111111111111111111111111111111111112".to_string(),
            output_lst_mint: "5oVNBeEEQvYi1cX3ir8Dx5n1P7pdxydbGF2X4TxVusJm".to_string(),
            amount: "1000000000".to_string(),
            mode: SwapMode::ExactIn,
            signer: "11111111111111111111111111111111".to_string(),
            swap_src: SwapSrc::SPool,
        };
        let s = serde_json::to_string(&p).expect("ser");
        assert!(s.contains("\"outputLstMint\""), "field should be camelCase: {s}");
        assert!(s.contains("\"swapSrc\":\"SPool\""));
        assert!(s.contains("\"mode\":\"ExactIn\""));
    }

    #[test]
    fn swap_resp_parses_minimal() {
        let json = r#"{"tx":"AQABAg=="}"#;
        let r: SwapResp = serde_json::from_str(json).expect("parse");
        assert_eq!(r.tx, "AQABAg==");
    }
}
