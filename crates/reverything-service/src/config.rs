//! Service settings, stored as `config.json` next to the indices. Only the service writes it,
//! clients change it through the pipe.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Drive letters of the indexed volumes. On the first run the Windows drive, so the app
    /// is not empty; the user picks them in the settings.
    pub volumes: Vec<char>,
}

impl Config {
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
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::first_run(),
            Err(_) => Self::default(),
        }
    }

    /// Nothing was chosen yet: index the drive Windows is installed on.
    fn first_run() -> Self {
        let system_drive = std::env::var("SystemDrive")
            .ok()
            .and_then(|d| d.chars().next())
            .filter(char::is_ascii_alphabetic)
            .map_or('C', |c| c.to_ascii_uppercase());
        log::info!("No drives chosen yet, indexing {}:", system_drive);
        Self {
            volumes: vec![system_drive],
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
