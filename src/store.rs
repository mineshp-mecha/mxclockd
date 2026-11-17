//! Persistence: alarms and timers live in $XDG_DATA_HOME/mxclockd/state.json.

use crate::model::{Alarm, Timer};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    pub alarms: Vec<Alarm>,
    pub timers: Vec<Timer>,
    /// Unix timestamp the scheduler last checked up to. Persisted so that
    /// after a power-off or crash we still see what came due in between.
    #[serde(default)]
    pub last_check: Option<i64>,
}

fn state_path() -> Result<PathBuf> {
    let dir = dirs::data_dir()
        .context("no XDG data dir")?
        .join("mxclockd");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("state.json"))
}

impl State {
    /// A file that doesn't parse is moved aside to `state.json.bad` and we
    /// start empty: losing the alarms is bad, but a daemon that refuses to
    /// start means no alarm ever rings again until someone fixes it by hand.
    pub fn load() -> Result<Self> {
        let path = state_path()?;
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e.into()),
        };
        match serde_json::from_slice(&bytes) {
            Ok(state) => Ok(state),
            Err(e) => {
                let bad = path.with_extension("json.bad");
                tracing::error!(
                    "{} is corrupt ({e}); moved to {} and starting with no alarms",
                    path.display(),
                    bad.display()
                );
                std::fs::rename(&path, &bad)
                    .with_context(|| format!("moving aside {}", path.display()))?;
                Ok(Self::default())
            }
        }
    }

    /// Write to a temp file and rename, so a crash mid-write never leaves a
    /// half-written state file behind. The fsyncs make that hold across a
    /// power cut too: without them the rename can reach the disk before the
    /// data, leaving an empty state.json.
    pub fn save(&self) -> Result<()> {
        let path = state_path()?;
        let tmp = path.with_extension("json.tmp");
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(self)?)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &path)?;
        if let Some(dir) = path.parent() {
            std::fs::File::open(dir)?.sync_all()?;
        }
        Ok(())
    }
}
