//! Minimal logger writing to stderr (console mode) or a file in the data directory (service).

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::SystemTime;

use log::{LevelFilter, Log, Metadata, Record};

/// Log files are started over once they get larger than this
const MAX_LOG_BYTES: u64 = 1024 * 1024;

struct Logger {
    file: Option<Mutex<File>>,
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let line = format!("[{}] {:5} {}\n", secs, record.level(), record.args());
        match &self.file {
            Some(file) => {
                let _ = file.lock().unwrap().write_all(line.as_bytes());
            }
            None => eprint!("{}", line),
        }
    }

    fn flush(&self) {}
}

pub fn init_stderr() {
    install(Logger { file: None });
}

pub fn init_file(path: &Path) {
    let append = std::fs::metadata(path).is_ok_and(|m| m.len() < MAX_LOG_BYTES);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(append)
        .write(true)
        .truncate(!append)
        .open(path)
        .ok();
    install(Logger {
        file: file.map(Mutex::new),
    });
}

fn install(logger: Logger) {
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(LevelFilter::Info);
    }
}
