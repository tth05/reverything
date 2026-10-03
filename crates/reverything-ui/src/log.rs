//! Tiny append-only log in `%LOCALAPPDATA%\Reverything\ui.log`, since the window has no console.

use std::io::Write;
use std::time::SystemTime;

pub fn write(message: &str) {
    let Some(dir) = std::env::var_os("LOCALAPPDATA") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir).join("Reverything");
    let _ = std::fs::create_dir_all(&dir);
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("ui.log"))
    {
        let _ = writeln!(file, "[{}] {}", secs, message);
    }
}
