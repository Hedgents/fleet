//! Compile-time threshold constants for signal classification.
//! Watchers compare current measurements against these to decide
//! Info/Notice/Important severity (or skip emission entirely).

/// Lending APR change >= this triggers Info severity.
pub const LENDING_RATE_INFO_DELTA_BPS: i16 = 50;
/// Lending APR change >= this triggers Notice severity.
pub const LENDING_RATE_NOTICE_DELTA_BPS: i16 = 200;

/// Perp funding rate at or above this threshold is interesting (Info).
pub const FUNDING_RATE_INFO_THRESHOLD_BPS: i32 = 500;
/// Perp funding above this is significant (Notice — speculator should consider entry).
pub const FUNDING_RATE_NOTICE_THRESHOLD_BPS: i32 = 2000;
/// Perp funding above this is extreme (Important — warrants alert + consider unwind in hedgedjlp).
pub const FUNDING_RATE_IMPORTANT_THRESHOLD_BPS: i32 = 5000;

/// 1h price move % triggering Notice severity.
pub const PRICE_1H_NOTICE_DELTA_BPS: i32 = 200;
/// 1h price move % triggering Important severity.
pub const PRICE_1H_IMPORTANT_DELTA_BPS: i32 = 500;

/// USDC/USDT depeg from $1.00 — Notice band.
pub const STABLE_DEPEG_NOTICE_BPS: i32 = 30;
/// Important band — fleet-wide pause should be considered.
pub const STABLE_DEPEG_IMPORTANT_BPS: i32 = 100;

/// Large-trade threshold for token activity signals (USDC lamports).
pub const LARGE_TRADE_NOTICE_USDC_LAMPORTS: u64 = 10_000_000_000; // $10k
pub const LARGE_TRADE_IMPORTANT_USDC_LAMPORTS: u64 = 100_000_000_000; // $100k

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_are_monotonic() {
        // Notice should be more demanding than Info (higher threshold).
        assert!(LENDING_RATE_NOTICE_DELTA_BPS > LENDING_RATE_INFO_DELTA_BPS);
        assert!(FUNDING_RATE_NOTICE_THRESHOLD_BPS > FUNDING_RATE_INFO_THRESHOLD_BPS);
        assert!(FUNDING_RATE_IMPORTANT_THRESHOLD_BPS > FUNDING_RATE_NOTICE_THRESHOLD_BPS);
        assert!(PRICE_1H_IMPORTANT_DELTA_BPS > PRICE_1H_NOTICE_DELTA_BPS);
        assert!(STABLE_DEPEG_IMPORTANT_BPS > STABLE_DEPEG_NOTICE_BPS);
        assert!(LARGE_TRADE_IMPORTANT_USDC_LAMPORTS > LARGE_TRADE_NOTICE_USDC_LAMPORTS);
    }

    #[test]
    fn thresholds_are_sane() {
        // 0 < Notice band < extreme. If anyone tunes these to 0 or
        // hundreds-of-percent, fail loudly.
        assert!(LENDING_RATE_NOTICE_DELTA_BPS > 0);
        assert!(LENDING_RATE_NOTICE_DELTA_BPS < 5000); // 50% APR change in one tick is unrealistic
        assert!(FUNDING_RATE_IMPORTANT_THRESHOLD_BPS < 100_000); // 1000% APR funding is absurd
    }
}
