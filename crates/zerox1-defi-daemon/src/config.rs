use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use solana_sdk::commitment_config::CommitmentConfig;

use crate::pairing::{FleetIdentity, Role};

#[derive(Debug, Parser)]
#[command(name = "zerox1-defi-daemon", version, about)]
pub struct Cli {
    /// Solana RPC URL. Defaults to mainnet.
    #[arg(long, env = "SOLANA_RPC_URL", default_value = "https://api.mainnet-beta.solana.com")]
    pub rpc_url: String,

    /// Path to wallet keypair JSON (Solana CLI format).
    #[arg(long, env = "WALLET_KEYPAIR_PATH")]
    pub wallet_keypair_path: Option<PathBuf>,

    /// Bind host. Default 127.0.0.1 — never expose remotely.
    #[arg(long, env = "DAEMON_BIND_HOST", default_value = "127.0.0.1")]
    pub bind_host: String,

    /// Bind port.
    #[arg(long, env = "DAEMON_BIND_PORT", default_value_t = 9091)]
    pub bind_port: u16,

    /// Solana commitment level (processed/confirmed/finalized).
    #[arg(long, env = "SOLANA_COMMITMENT", default_value = "confirmed")]
    pub commitment: String,

    /// Persistent data directory (pairing state, future caches).
    #[arg(long, env = "DAEMON_DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    // ── Fleet pairing flags (all four required to enable pairing) ───────────

    /// 16-hex fleet id (8 bytes). Names the fleet for routing purposes.
    #[arg(long, env = "FLEET_ID")]
    pub fleet_id: Option<String>,

    /// 64-hex fleet token (32 bytes). Shared secret for HMAC. Use --fleet-token-file
    /// in production to avoid shell history.
    #[arg(long, env = "FLEET_TOKEN", hide_env_values = true)]
    pub fleet_token: Option<String>,

    /// File containing the 64-hex fleet token. Preferred over --fleet-token.
    #[arg(long, env = "FLEET_TOKEN_FILE")]
    pub fleet_token_file: Option<PathBuf>,

    /// Worker role: orchestrator | multiply | hedgedJlp | stableFloor |
    /// riskWatcher | researcher | speculator
    #[arg(long, env = "FLEET_ROLE")]
    pub role: Option<String>,
}

pub struct Config {
    pub rpc_url: String,
    pub wallet_path: PathBuf,
    pub bind_host: String,
    pub bind_port: u16,
    pub commitment: CommitmentConfig,
    pub data_dir: PathBuf,
    /// `Some` iff all four fleet flags were supplied (fleet_id + token + role + agent_id).
    pub fleet_identity_partial: Option<FleetIdentityPartial>,
}

/// Fleet identity minus the agent_id, which is derived from the wallet.
pub struct FleetIdentityPartial {
    pub fleet_id: [u8; 8],
    pub fleet_token: [u8; 32],
    pub role: Role,
}

impl FleetIdentityPartial {
    pub fn complete(self, agent_id: String) -> FleetIdentity {
        FleetIdentity {
            fleet_id: self.fleet_id,
            fleet_token: self.fleet_token,
            role: self.role,
            agent_id,
        }
    }
}

impl Cli {
    pub fn into_config(self) -> Result<Config> {
        let wallet_path = match self.wallet_keypair_path {
            Some(p) => p,
            None => {
                let home = std::env::var("HOME").map_err(|_| anyhow!("HOME not set"))?;
                PathBuf::from(home).join(".config/solana/id.json")
            }
        };

        let commitment = match self.commitment.as_str() {
            "processed" => CommitmentConfig::processed(),
            "confirmed" => CommitmentConfig::confirmed(),
            "finalized" => CommitmentConfig::finalized(),
            other => return Err(anyhow!("invalid commitment: {other}")),
        };

        if self.bind_host != "127.0.0.1" && self.bind_host != "::1" {
            tracing::warn!(
                host = %self.bind_host,
                "binding to non-loopback host — wallet exposure risk"
            );
        }

        let data_dir = match self.data_dir {
            Some(p) => p,
            None => {
                let home = std::env::var("HOME").map_err(|_| anyhow!("HOME not set"))?;
                PathBuf::from(home).join(".zerox1-defi")
            }
        };
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("create data dir {}", data_dir.display()))?;

        let fleet_identity_partial = parse_fleet_identity(
            self.fleet_id, self.fleet_token, self.fleet_token_file, self.role,
        )?;

        Ok(Config {
            rpc_url: self.rpc_url,
            wallet_path,
            bind_host: self.bind_host,
            bind_port: self.bind_port,
            commitment,
            data_dir,
            fleet_identity_partial,
        })
    }
}

fn parse_fleet_identity(
    fleet_id: Option<String>,
    fleet_token: Option<String>,
    fleet_token_file: Option<PathBuf>,
    role: Option<String>,
) -> Result<Option<FleetIdentityPartial>> {
    // All four absent: pairing disabled (legitimate for dev/testing).
    if fleet_id.is_none() && fleet_token.is_none() && fleet_token_file.is_none() && role.is_none() {
        return Ok(None);
    }

    let fleet_id = fleet_id.ok_or_else(|| anyhow!("--fleet-id required when fleet pairing flags are used"))?;
    let role = role.ok_or_else(|| anyhow!("--role required when fleet pairing flags are used"))?;

    let token_hex = match (fleet_token, fleet_token_file) {
        (Some(_), Some(_)) => {
            return Err(anyhow!("specify either --fleet-token or --fleet-token-file, not both"));
        }
        (Some(t), None) => t,
        (None, Some(p)) => std::fs::read_to_string(&p)
            .with_context(|| format!("read fleet token file {}", p.display()))?
            .trim()
            .to_string(),
        (None, None) => {
            return Err(anyhow!("--fleet-token or --fleet-token-file required when fleet pairing flags are used"));
        }
    };

    let id_bytes = hex::decode(fleet_id.trim())
        .map_err(|e| anyhow!("--fleet-id must be 16-hex chars: {e}"))?;
    if id_bytes.len() != 8 {
        return Err(anyhow!("--fleet-id must decode to exactly 8 bytes (16 hex chars)"));
    }
    let mut fleet_id = [0u8; 8];
    fleet_id.copy_from_slice(&id_bytes);

    let token_bytes = hex::decode(token_hex.trim())
        .map_err(|e| anyhow!("--fleet-token must be 64-hex chars: {e}"))?;
    if token_bytes.len() != 32 {
        return Err(anyhow!("--fleet-token must decode to exactly 32 bytes (64 hex chars)"));
    }
    let mut fleet_token = [0u8; 32];
    fleet_token.copy_from_slice(&token_bytes);

    let role = Role::from_str(&role).map_err(|e| anyhow!(e.to_string()))?;

    Ok(Some(FleetIdentityPartial { fleet_id, fleet_token, role }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_returns_none_when_no_flags() {
        let r = parse_fleet_identity(None, None, None, None).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn parse_requires_all_flags_when_any_provided() {
        let r = parse_fleet_identity(Some("0102030405060708".into()), None, None, None);
        assert!(r.is_err());
    }

    #[test]
    fn parse_rejects_token_and_token_file_together() {
        let r = parse_fleet_identity(
            Some("0102030405060708".into()),
            Some("00".repeat(32)),
            Some("/tmp/x".into()),
            Some("multiply".into()),
        );
        assert!(r.is_err());
    }

    #[test]
    fn parse_rejects_wrong_id_length() {
        let r = parse_fleet_identity(
            Some("0102".into()),
            Some("00".repeat(32)),
            None,
            Some("multiply".into()),
        );
        assert!(r.is_err());
    }

    #[test]
    fn parse_rejects_wrong_token_length() {
        let r = parse_fleet_identity(
            Some("0102030405060708".into()),
            Some("00".repeat(16)),
            None,
            Some("multiply".into()),
        );
        assert!(r.is_err());
    }

    #[test]
    fn parse_succeeds_with_token_inline() {
        let r = parse_fleet_identity(
            Some("0102030405060708".into()),
            Some("ab".repeat(32)),
            None,
            Some("multiply".into()),
        ).unwrap();
        let id = r.unwrap();
        assert_eq!(id.fleet_id[0], 0x01);
        assert_eq!(id.fleet_token[0], 0xab);
        assert_eq!(id.role, Role::Multiply);
    }
}
