use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid pubkey: {0}")]
    InvalidPubkey(String),

    #[error("amount must be greater than zero")]
    ZeroAmount,

    #[error("amount overflow")]
    Overflow,

    #[error("unknown asset symbol: {0}")]
    UnknownAsset(String),

    #[error("unknown reserve for asset {asset} on market {market}")]
    UnknownReserve { asset: String, market: String },

    #[error("not yet implemented: {0}")]
    NotImplemented(&'static str),
}
