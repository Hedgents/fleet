use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

pub fn program_ids() -> Vec<Pubkey> {
    vec![
        // Jupiter Perps / JLP
        Pubkey::from_str("PERPHjGBqRHArX4DySjwM6UJHiR3sWAatqfdBS2qQJu").unwrap(),
        // Adrena
        Pubkey::from_str("13gDzEXCdocbj8iAiqrScGo47NiSuYENGsRqi3SEAwet").unwrap(),
    ]
}
