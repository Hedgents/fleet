//! Secret-source abstraction.
//!
//! Daemons load their role identity (and Solana wallet, eventually) from
//! a `SecretSource` at boot. The actual storage backend is configurable —
//! file path for development, env var for containers, Vault for prod.
//!
//! For the fleet we ship `FileSource` and `EnvSource` here. Vault / SOPS /
//! sealed-secret backends are out of scope for the core runtime crate;
//! they would live in a separate `runtime-secrets-vault` crate gated by
//! feature flags so non-prod deployments don't pull the dependency.

use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;

use crate::identity::{Role, RoleIdentity};

#[async_trait::async_trait]
pub trait SecretSource: Send + Sync {
    /// Fetch a secret by logical name. Returns the raw bytes; callers parse.
    /// The name is a backend-agnostic key (e.g. "multiply-role.key" maps
    /// to a file in `FileSource` or an env var like `MULTIPLY_ROLE_KEY`
    /// in `EnvSource`).
    async fn get(&self, name: &str) -> Result<Vec<u8>>;
}

/// Reads secrets from files in a directory. Each secret is a separate
/// file named after the secret key (e.g. `multiply-role.key`).
pub struct FileSource {
    base: PathBuf,
}

impl FileSource {
    pub fn new(base: impl Into<PathBuf>) -> Self {
        Self { base: base.into() }
    }
}

#[async_trait::async_trait]
impl SecretSource for FileSource {
    async fn get(&self, name: &str) -> Result<Vec<u8>> {
        let path = self.base.join(name);
        tokio::fs::read(&path)
            .await
            .with_context(|| format!("read secret {} at {}", name, path.display()))
    }
}

/// Reads secrets from environment variables. Useful for container
/// deployments where the secret is injected by the orchestrator. The
/// name is uppercased and dashes are converted to underscores
/// (`multiply-role.key` -> `MULTIPLY_ROLE_KEY`; the dot is also
/// replaced with underscore).
pub struct EnvSource;

impl EnvSource {
    fn env_var_name(secret_name: &str) -> String {
        secret_name.to_uppercase().replace(['-', '.'], "_")
    }
}

#[async_trait::async_trait]
impl SecretSource for EnvSource {
    async fn get(&self, name: &str) -> Result<Vec<u8>> {
        let var = Self::env_var_name(name);
        let value = std::env::var(&var).with_context(|| format!("env var {} not set", var))?;
        Ok(value.into_bytes())
    }
}

/// Load a role identity from a secret source. The secret must be
/// exactly 32 raw bytes (Ed25519 signing-key seed). Generate with
/// e.g. `openssl rand 32 > /path/to/multiply-role.key`.
///
/// The `secret_name` is the backend-agnostic key — `FileSource` looks
/// up `<base>/<secret_name>`, `EnvSource` looks up an environment
/// variable derived from the same name.
pub async fn load_role_identity(
    source: &dyn SecretSource,
    role: Role,
    secret_name: &str,
) -> Result<RoleIdentity> {
    let raw = source.get(secret_name).await?;
    if raw.len() != 32 {
        return Err(anyhow!(
            "role key for {} must be exactly 32 bytes, got {}",
            role.as_str(),
            raw.len()
        ));
    }
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&raw);
    Ok(RoleIdentity::new(role, seed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn file_source_round_trip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("multiply-role.key");
        let key_bytes: [u8; 32] = [42; 32];
        tokio::fs::write(&path, key_bytes).await.unwrap();

        let src = FileSource::new(tmp.path());
        let id = load_role_identity(&src, Role::Multiply, "multiply-role.key")
            .await
            .unwrap();
        assert_eq!(id.role(), Role::Multiply);
        assert_eq!(id.signing_key_bytes(), &key_bytes);
    }

    #[tokio::test]
    async fn file_source_missing_file_errors() {
        let tmp = TempDir::new().unwrap();
        let src = FileSource::new(tmp.path());
        let res = load_role_identity(&src, Role::Multiply, "nonexistent.key").await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn rejects_wrong_size() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad.key");
        tokio::fs::write(&path, b"too short").await.unwrap();

        let src = FileSource::new(tmp.path());
        let res = load_role_identity(&src, Role::Multiply, "bad.key").await;
        assert!(res.is_err());
        let msg = format!("{:#}", res.unwrap_err());
        assert!(msg.contains("32 bytes"));
    }

    #[test]
    fn env_var_name_conversion() {
        assert_eq!(
            EnvSource::env_var_name("multiply-role.key"),
            "MULTIPLY_ROLE_KEY"
        );
        assert_eq!(EnvSource::env_var_name("foo"), "FOO");
        assert_eq!(EnvSource::env_var_name("a-b-c.d"), "A_B_C_D");
    }
}
