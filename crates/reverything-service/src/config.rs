//! Service settings, stored as `config.json` next to the indices. Only the service writes it,
//! clients change it through the pipe.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Drive letters of the indexed volumes. None by default, the user picks them in the app.
    pub volumes: Vec<char>,
    /// Unload the indices after no client was active for this long, 0 for never
    pub unload_after_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            volumes: Vec::new(),
            unload_after_secs: reverything_core::service::UNLOAD_AFTER.as_secs(),
        }
    }
}

impl Config {
    /// The current settings of a running service.
    pub fn of(set: &reverything_core::service::IndexSet) -> Self {
        Self {
            volumes: set.enabled(),
            unload_after_secs: set.unload_after().map_or(0, |d| d.as_secs()),
        }
    }

    pub fn unload_after(&self) -> Option<std::time::Duration> {
        (self.unload_after_secs > 0).then(|| std::time::Duration::from_secs(self.unload_after_secs))
    }

    fn path(dir: &Path) -> PathBuf {
        dir.join("config.json")
    }

    pub fn load(dir: &Path) -> Self {
        let path = Self::path(dir);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                log::warn!("Ignoring invalid {}: {}", path.display(), e);
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let path = Self::path(dir);
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &path)
    }
}
