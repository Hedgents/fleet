//! Compile-time threshold constants for signal classification.
//! Watchers compare current measurements against these to decide
//! Info/Notice/Important severity (or skip emission entirely).

/// Lending APR change >= this triggers Info severity.
pub const LENDING_RATE_INFO_DELTA_BPS: i16 = 50;
/// Lending APR change >= this triggers Notice severity.
pub const LENDING_RATE_NOTICE_DELTA_BPS: i16 = 200;

/// 1h price move % triggering Notice severity.
pub const PRICE_1H_NOTICE_DELTA_BPS: i32 = 100;
/// 1h price move % triggering Important severity.
pub const PRICE_1H_IMPORTANT_DELTA_BPS: i32 = 300;

/// USDC/USDT depeg from $1.00 — Notice band.
pub const STABLE_DEPEG_NOTICE_BPS: i32 = 30;
/// Important band — fleet-wide pause should be considered.
pub const STABLE_DEPEG_IMPORTANT_BPS: i32 = 100;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_are_monotonic() {
        // Notice should be more demanding than Info (higher threshold).
        assert!(LENDING_RATE_NOTICE_DELTA_BPS > LENDING_RATE_INFO_DELTA_BPS);
        assert!(PRICE_1H_IMPORTANT_DELTA_BPS > PRICE_1H_NOTICE_DELTA_BPS);
        assert!(STABLE_DEPEG_IMPORTANT_BPS > STABLE_DEPEG_NOTICE_BPS);
    }

    #[test]
    fn thresholds_are_sane() {
        // 0 < Notice band < extreme. If anyone tunes these to 0 or
        // hundreds-of-percent, fail loudly.
        assert!(LENDING_RATE_NOTICE_DELTA_BPS > 0);
        assert!(LENDING_RATE_NOTICE_DELTA_BPS < 5000); // 50% APR change in one tick is unrealistic
    }
}
