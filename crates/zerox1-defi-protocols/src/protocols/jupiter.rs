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
//!
//! v0.2.3 adds [`JupiterSwap`] — a reusable HTTP client wrapper used by
//! the hedgedjlp-daemon to route USDC↔JLP through the aggregator instead
//! of constructing `add_liquidity_2` / `remove_liquidity_2` directly
//! (the direct Anchor path is effectively dead per the May 2026 audit
//! in `docs/jupiter-perps-bundle-spec.md` §2).

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::{Deserialize, Serialize};
use solana_sdk::{pubkey::Pubkey, transaction::VersionedTransaction};
use std::time::Duration;

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

// ── JupiterSwap HTTP client ──────────────────────────────────────────────────
//
// Minimal wrapper around the v1 Jupiter Swap API. Two public methods:
//   * `quote(QuoteRequest) -> SwapQuote`
//   * `swap(SwapRequest)  -> SwapResponse`
//
// Plus two convenience helpers for the hedgedjlp daemon:
//   * `build_jlp_buy_tx`   — USDC → JLP via the aggregator
//   * `build_jlp_redeem_tx` — JLP → USDC via the aggregator
//
// Both return a fully-formed `VersionedTransaction` with Jupiter's chosen
// ALTs baked in. The caller signs the user-pubkey slot and either
// simulates or broadcasts via `RpcContext::sign_existing_simulate /
// sign_existing_send` (added to the runtime crate in v0.2.3).
//
// The lite endpoint (`lite-api.jup.ag`) is keyless and rate-limited;
// the `swap()` method retries once on a 429 with a small backoff so a
// transient burst does not surface as a hard failure to callers.

/// Default request timeout for the Jupiter HTTP client. The lite endpoint
/// usually responds in well under a second; 10s is a generous ceiling that
/// still surfaces a hung connection.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// One retry on `429 Too Many Requests` before surfacing the error.
const RATE_LIMIT_RETRY_DELAY: Duration = Duration::from_millis(750);

#[derive(Debug, Clone)]
pub struct QuoteRequest {
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    pub amount_lamports: u64,
    pub slippage_bps: u16,
}

#[derive(Debug, Clone)]
pub struct SwapRequest {
    /// The opaque quote object returned by [`JupiterSwap::quote`].
    pub quote_response: SwapQuote,
    pub user_public_key: Pubkey,
    /// `false` for SPL-only routes (USDC ↔ JLP); `true` when a SOL leg
    /// appears in the route.
    pub wrap_and_unwrap_sol: bool,
}

#[derive(Debug, Clone)]
pub struct SwapResponse {
    /// The raw, bincode-serialized `VersionedTransaction` bytes — already
    /// b64-decoded. Callers can either keep the bytes or use
    /// [`SwapResponse::into_versioned_tx`].
    pub swap_transaction: Vec<u8>,
    pub last_valid_block_height: u64,
    pub prioritization_fee_lamports: u64,
    pub compute_unit_limit: u32,
}

impl SwapResponse {
    /// Bincode-deserialize the raw tx into a `VersionedTransaction`. The
    /// signature slot for `user_public_key` will be empty (zeroed) —
    /// caller must sign before broadcast.
    pub fn into_versioned_tx(&self) -> Result<VersionedTransaction> {
        bincode::deserialize::<VersionedTransaction>(&self.swap_transaction)
            .map_err(|e| anyhow!("deserialize jupiter swap_transaction: {e}"))
    }
}

/// HTTP client wrapper around the Jupiter Swap v1 API. Holds a single
/// `reqwest::Client` and a base URL — clone-cheap, share across tasks.
#[derive(Debug, Clone)]
pub struct JupiterSwap {
    base_url: String,
    http: reqwest::Client,
}

impl Default for JupiterSwap {
    fn default() -> Self {
        Self::new_lite()
    }
}

impl JupiterSwap {
    /// Construct against the public lite endpoint (no API key required).
    pub fn new_lite() -> Self {
        Self::new(format!("{JUPITER_LITE_API}/swap/v1"))
    }

    /// Construct with a custom base URL — primarily for tests that point
    /// at a mock server. The URL should NOT include a trailing slash and
    /// should already include the `/swap/v1` path prefix.
    pub fn new(base_url: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .expect("reqwest client build");
        Self { base_url, http }
    }

    /// `GET /swap/v1/quote`. Returns the opaque quote object verbatim —
    /// must be passed back to [`JupiterSwap::swap`] without modification.
    pub async fn quote(&self, req: QuoteRequest) -> Result<SwapQuote> {
        let amount = req.amount_lamports.to_string();
        let slippage = req.slippage_bps.to_string();
        let input = req.input_mint.to_string();
        let output = req.output_mint.to_string();
        let url = format!("{}/quote", self.base_url);
        let resp = self
            .http
            .get(&url)
            .query(&[
                ("inputMint", input.as_str()),
                ("outputMint", output.as_str()),
                ("amount", amount.as_str()),
                ("slippageBps", slippage.as_str()),
            ])
            .send()
            .await
            .context("jupiter quote http send")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "jupiter quote returned {}: {}",
                status,
                truncate_body(&body)
            ));
        }
        resp.json::<SwapQuote>()
            .await
            .context("jupiter quote json decode")
    }

    /// `POST /swap/v1/swap`. Returns the (already base64-decoded) tx bytes.
    /// Retries once on `429 Too Many Requests`.
    pub async fn swap(&self, req: SwapRequest) -> Result<SwapResponse> {
        let body = SwapBuildParams {
            quote_response: &req.quote_response,
            user_public_key: req.user_public_key.to_string(),
            wrap_and_unwrap_sol: req.wrap_and_unwrap_sol,
        };
        let url = format!("{}/swap", self.base_url);

        // First attempt + single 429 retry.
        let resp = match self.http.post(&url).json(&body).send().await {
            Ok(r) if r.status() == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                tokio::time::sleep(RATE_LIMIT_RETRY_DELAY).await;
                self.http
                    .post(&url)
                    .json(&body)
                    .send()
                    .await
                    .context("jupiter swap http send (retry)")?
            }
            Ok(r) => r,
            Err(e) => return Err(anyhow!("jupiter swap http send: {e}")),
        };

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "jupiter swap returned {}: {}",
                status,
                truncate_body(&body)
            ));
        }
        let parsed: SwapBuildResp = resp
            .json()
            .await
            .context("jupiter swap json decode")?;
        let bytes = B64
            .decode(&parsed.swap_transaction)
            .map_err(|e| anyhow!("decode swap_transaction base64: {e}"))?;
        Ok(SwapResponse {
            swap_transaction: bytes,
            last_valid_block_height: parsed.last_valid_block_height,
            prioritization_fee_lamports: parsed.prioritization_fee_lamports,
            compute_unit_limit: parsed.compute_unit_limit,
        })
    }
}

fn truncate_body(s: &str) -> String {
    if s.len() <= 240 {
        s.to_string()
    } else {
        format!("{}…", &s[..240])
    }
}

/// Build a `USDC → JLP` swap transaction via the Jupiter aggregator.
/// The returned tx has Jupiter's selected ALTs baked in; the signature
/// slot for `user` is empty — caller must sign and either simulate or
/// broadcast. The route may pass through any of Jupiter's market
/// integrations (Whirlpool, Meteora DLMM, GoonFi, Phoenix, etc.).
pub async fn build_jlp_buy_tx(
    jup: &JupiterSwap,
    user: &Pubkey,
    usdc_lamports: u64,
    slippage_bps: u16,
) -> Result<VersionedTransaction> {
    if usdc_lamports == 0 {
        return Err(anyhow!("usdc_lamports must be > 0"));
    }
    let quote = jup
        .quote(QuoteRequest {
            input_mint: crate::constants::USDC_MINT,
            output_mint: crate::constants::JLP_MINT,
            amount_lamports: usdc_lamports,
            slippage_bps,
        })
        .await
        .context("jupiter quote USDC->JLP")?;
    let swap = jup
        .swap(SwapRequest {
            quote_response: quote,
            user_public_key: *user,
            wrap_and_unwrap_sol: false,
        })
        .await
        .context("jupiter swap USDC->JLP")?;
    swap.into_versioned_tx()
}

/// Build a `JLP → USDC` swap transaction via the Jupiter aggregator.
/// Symmetric to [`build_jlp_buy_tx`].
pub async fn build_jlp_redeem_tx(
    jup: &JupiterSwap,
    user: &Pubkey,
    jlp_lamports: u64,
    slippage_bps: u16,
) -> Result<VersionedTransaction> {
    if jlp_lamports == 0 {
        return Err(anyhow!("jlp_lamports must be > 0"));
    }
    let quote = jup
        .quote(QuoteRequest {
            input_mint: crate::constants::JLP_MINT,
            output_mint: crate::constants::USDC_MINT,
            amount_lamports: jlp_lamports,
            slippage_bps,
        })
        .await
        .context("jupiter quote JLP->USDC")?;
    let swap = jup
        .swap(SwapRequest {
            quote_response: quote,
            user_public_key: *user,
            wrap_and_unwrap_sol: false,
        })
        .await
        .context("jupiter swap JLP->USDC")?;
    swap.into_versioned_tx()
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
    fn truncate_body_leaves_short_strings_alone() {
        assert_eq!(truncate_body("hello"), "hello");
    }

    #[test]
    fn truncate_body_caps_long_strings() {
        let s = "x".repeat(500);
        let t = truncate_body(&s);
        assert!(t.len() <= 250); // 240 chars + ellipsis byte width
        assert!(t.ends_with('…'));
    }

    #[test]
    fn jupiter_swap_lite_default_targets_lite_endpoint() {
        let c = JupiterSwap::default();
        assert!(c.base_url.contains("lite-api.jup.ag"));
        assert!(c.base_url.ends_with("/swap/v1"));
    }

    #[test]
    fn swap_response_round_trips_a_versioned_tx() {
        use solana_sdk::hash::Hash;
        use solana_sdk::message::{v0::Message as V0Message, VersionedMessage};
        use solana_sdk::pubkey::Pubkey;
        // Build a trivial empty v0 message and bincode it as the raw tx
        // payload; check that SwapResponse::into_versioned_tx restores it.
        let payer = Pubkey::new_unique();
        let msg = V0Message::try_compile(&payer, &[], &[], Hash::default()).unwrap();
        let tx = VersionedTransaction {
            signatures: vec![Default::default()],
            message: VersionedMessage::V0(msg),
        };
        let bytes = bincode::serialize(&tx).unwrap();
        let resp = SwapResponse {
            swap_transaction: bytes,
            last_valid_block_height: 1234,
            prioritization_fee_lamports: 0,
            compute_unit_limit: 1_400_000,
        };
        let back = resp.into_versioned_tx().expect("deserialize");
        assert_eq!(back.signatures.len(), 1);
    }

    #[tokio::test]
    async fn build_jlp_buy_tx_rejects_zero_amount() {
        let jup = JupiterSwap::new_lite();
        let user = solana_sdk::pubkey::Pubkey::new_unique();
        let res = build_jlp_buy_tx(&jup, &user, 0, 50).await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("usdc_lamports"));
    }

    #[tokio::test]
    async fn build_jlp_redeem_tx_rejects_zero_amount() {
        let jup = JupiterSwap::new_lite();
        let user = solana_sdk::pubkey::Pubkey::new_unique();
        let res = build_jlp_redeem_tx(&jup, &user, 0, 50).await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("jlp_lamports"));
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
