use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing::warn;

#[derive(Serialize, Deserialize, Debug)]
pub struct LegPair {
    pub pair_id: String,
    pub long_sig:  Option<String>,
    pub short_sig: Option<String>,
    pub state:     LegState,
    pub ts:        i64,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum LegState { Pending, BothFilled, OrphanLong, OrphanShort, Closed }

pub struct Ledger {
    path: PathBuf,
    file: Mutex<std::fs::File>,
}

impl Ledger {
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new().create(true).append(true).read(true).open(path)?;
        Ok(Self { path: path.to_path_buf(), file: Mutex::new(file) })
    }

    pub fn append(&self, entry: &LegPair) -> Result<()> {
        let mut f = self.file.lock().unwrap();
        writeln!(f, "{}", serde_json::to_string(entry)?)?;
        f.flush()?;
        Ok(())
    }

    /// On boot, scan the ledger and report any pair stuck in OrphanLong /
    /// OrphanShort — the close path is implemented in the strategy
    /// follow-up plan; this just surfaces them.
    pub async fn recover_orphans(&self) -> Result<()> {
        let f = std::fs::File::open(&self.path)?;
        for line in BufReader::new(f).lines() {
            let line = line?;
            let entry: LegPair = match serde_json::from_str(&line) {
                Ok(e) => e,
                Err(_) => continue,
            };
            if matches!(entry.state, LegState::OrphanLong | LegState::OrphanShort) {
                warn!(pair = %entry.pair_id, state = ?entry.state, "ledger orphan needs close");
            }
        }
        Ok(())
    }
}
