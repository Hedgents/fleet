//! Persist pairing state across daemon restarts.
//!
//! Single JSON file at `<data_dir>/fleet.json`. Atomic write via temp+rename.
//! Read on startup; written on every state transition. Tiny enough that we
//! do not need a database.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::pairing::PairingState;

#[derive(Clone)]
pub struct StateFile {
    path: PathBuf,
}

impl StateFile {
    pub fn new(data_dir: &Path) -> Self {
        Self { path: data_dir.join("fleet.json") }
    }

    pub fn load(&self) -> Result<PairingState> {
        if !self.path.exists() {
            return Ok(PairingState::Unpaired);
        }
        let raw = std::fs::read_to_string(&self.path)
            .with_context(|| format!("read {}", self.path.display()))?;
        let state: PairingState = serde_json::from_str(&raw)
            .with_context(|| format!("parse {}", self.path.display()))?;
        Ok(state)
    }

    pub fn save(&self, state: &PairingState) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(state).context("serialize state")?;
        std::fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "zerox1-defi-test-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn load_returns_unpaired_when_missing() {
        let d = tmpdir();
        let f = StateFile::new(&d);
        assert!(matches!(f.load().unwrap(), PairingState::Unpaired));
    }

    #[test]
    fn save_then_load_roundtrips() {
        let d = tmpdir();
        let f = StateFile::new(&d);
        let s = PairingState::Paired {
            orchestrator_agent_id: "OrchAbc".into(),
            paired_at: 1234,
        };
        f.save(&s).unwrap();
        let loaded = f.load().unwrap();
        assert_eq!(loaded, s);
    }

    #[test]
    fn save_is_atomic() {
        let d = tmpdir();
        let f = StateFile::new(&d);
        f.save(&PairingState::Unpaired).unwrap();
        // No leftover .tmp file after save
        let tmp_left = std::fs::read_dir(&d).unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!tmp_left, "atomic rename should leave no .tmp file");
    }
}
