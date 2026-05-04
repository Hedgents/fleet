use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

pub fn program_ids() -> Vec<Pubkey> {
    vec![
        // Jupiter Aggregator v6
        Pubkey::from_str("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4").unwrap(),
        // SPL Token (transfers)
        Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap(),
    ]
}
