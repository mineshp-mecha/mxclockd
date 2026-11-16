//! Persistence: alarms and timers live in $XDG_DATA_HOME/clockd/state.json.

use crate::model::{Alarm, Timer};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    pub alarms: Vec<Alarm>,
    pub timers: Vec<Timer>,
}

fn state_path() -> Result<PathBuf> {
    let dir = dirs::data_dir()
        .context("no XDG data dir")?
        .join("clockd");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("state.json"))
}

impl State {
    pub fn load() -> Result<Self> {
        let path = state_path()?;
        match std::fs::read_to_string(&path) {
            Ok(s) => Ok(serde_json::from_str(&s)
                .with_context(|| format!("parsing {}", path.display()))?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Write to a temp file and rename, so a crash mid-write
    /// never leaves a half-written state file behind.
    pub fn save(&self) -> Result<()> {
        let path = state_path()?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}
