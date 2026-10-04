//! Tiny append-only log in `%LOCALAPPDATA%\Reverything\ui.log`, since the window has no console.
//! It is moved to `ui.log.old` once it gets larger than [`MAX_LOG_BYTES`].

use std::io::Write;
use std::time::SystemTime;

const MAX_LOG_BYTES: u64 = 1024 * 1024;

pub fn write(message: &str) {
    let Some(dir) = std::env::var_os("LOCALAPPDATA") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir).join("Reverything");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("ui.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let _ = std::fs::rename(&path, dir.join("ui.log.old"));
    }
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "[{}] {}", secs, message);
    }
}
