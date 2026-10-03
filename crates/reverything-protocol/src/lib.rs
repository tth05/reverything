//! Messages between the Reverything service and its clients.
//!
//! The service listens on [`PIPE_NAME`]. Every message is a little endian `u32` length followed
//! by that many bytes of postcard encoded data. The client sends a [`Request`] and reads exactly
//! one [`Response`] before sending the next request.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub const PIPE_NAME: &str = r"\\.\pipe\reverything";

/// Environment variable with another pipe name, for running a development service next to the
/// installed one. The installed service ignores it.
pub const PIPE_ENV: &str = "REVERYTHING_PIPE";

/// [`PIPE_NAME`], or the pipe named in [`PIPE_ENV`].
pub fn pipe_name() -> String {
    std::env::var(PIPE_ENV)
        .ok()
        .filter(|name| name.starts_with(r"\\.\pipe\") && name.len() > 9)
        .unwrap_or_else(|| PIPE_NAME.to_string())
}

/// Bumped on incompatible changes. Clients and the service have to agree on it.
pub const PROTOCOL_VERSION: u32 = 5;

/// Upper bound for requests, which come from less privileged processes
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
/// Upper bound for responses
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
/// Most rows a single [`Request::Rows`] may ask for
pub const MAX_ROWS_PER_REQUEST: u32 = 2048;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Hello {
        version: u32,
    },
    /// Runs a search. It replaces the previous result set of this connection. See the query
    /// syntax in `reverything_core::index::search`.
    Search {
        query: String,
        sort: Sort,
        /// Include files
        files: bool,
        /// Include folders
        folders: bool,
    },
    /// Rows `start..start + count` of the result set with id `search`
    Rows {
        search: u64,
        start: u64,
        count: u32,
    },
    Status,
    /// Indexes exactly these volumes (drive letters). The choice is saved by the service.
    SetVolumes {
        volumes: Vec<char>,
    },
    /// The client's window got (`true`) or lost the focus. The indices are only loaded and
    /// updated live while a client is active. Clients that never send this count as active
    /// from their first search on.
    SetActive {
        active: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Hello {
        version: u32,
    },
    Search {
        search: u64,
        total: u64,
        took_us: u64,
    },
    Rows {
        search: u64,
        start: u64,
        rows: Vec<Row>,
    },
    Status(Status),
    Done,
    Error(String),
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SortColumn {
    /// Best matches first, see `reverything_core::index::rank`
    #[default]
    Relevance,
    Name,
    Path,
    Size,
    Modified,
    Created,
    Attributes,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sort {
    pub column: SortColumn,
    pub ascending: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Self {
            column: SortColumn::Relevance,
            ascending: true,
        }
    }
}

/// A search result. Formatting (dates, sizes) is left to the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Row {
    pub name: String,
    /// Path of the containing directory, e.g. `C:\Windows`
    pub folder: String,
    /// File size, or the total size of everything below a directory
    pub size: u64,
    pub directory: bool,
    /// Unix timestamps in seconds
    pub modified: u32,
    pub created: u32,
    /// FILE_ATTRIBUTE_* flags
    pub attributes: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VolumeState {
    /// Not indexed
    Disabled,
    Waiting,
    Loading,
    Indexing,
    Ready,
    /// Unloaded while the app is not used, loaded again when it is
    Asleep,
    Offline,
    Failed(String),
}

/// Durations are in microseconds, times in seconds since the unix epoch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub service_version: String,
    pub uptime_us: u64,
    /// Changes whenever any index changes
    pub generation: u64,
    pub working_set_bytes: u64,
    pub private_bytes: u64,
    pub searches: u64,
    pub last_search_us: Option<u64>,
    /// Every NTFS volume, including the ones that are not indexed
    pub volumes: Vec<VolumeStatus>,
}

/// What happened to the saved index when the volume was enabled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SavedIndex {
    NotChecked,
    /// There was none, so the volume was scanned
    Missing,
    Loaded,
    /// It could not be used, so the volume was scanned
    Discarded(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeStatus {
    pub letter: char,
    pub state: VolumeState,
    pub entries: u64,
    pub index_bytes: u64,
    pub load_us: Option<u64>,
    pub saved_index: SavedIndex,
    pub scan: Option<ScanTimings>,
    pub catch_up: Option<BatchTimings>,
    pub last_batch: Option<BatchTimings>,
    pub batches: u64,
    pub records_updated: u64,
    pub last_save: Option<SaveTimings>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanTimings {
    pub open_us: u64,
    pub read_parse_us: u64,
    pub merge_us: u64,
    pub sort_us: u64,
    pub folder_sizes_us: u64,
    pub bytes_read: u64,
    pub records: u64,
    pub links: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchTimings {
    pub records: u64,
    pub updates: u64,
    pub fetch_us: u64,
    pub apply_us: u64,
    pub at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaveTimings {
    pub took_us: u64,
    pub bytes: u64,
    pub at: u64,
}

pub fn write_message<T: Serialize>(w: &mut impl Write, message: &T) -> io::Result<()> {
    let bytes = postcard::to_stdvec(message).map_err(io::Error::other)?;
    let len = u32::try_from(bytes.len()).map_err(io::Error::other)?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&bytes)?;
    w.flush()
}

/// Reads one message, refusing anything larger than `max_bytes`.
pub fn read_message<T: DeserializeOwned>(r: &mut impl Read, max_bytes: usize) -> io::Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Message of {} bytes exceeds the limit of {} bytes",
                len, max_bytes
            ),
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    postcard::from_bytes(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Blocking client connection to the service.
pub struct Client {
    reader: io::BufReader<std::fs::File>,
    writer: io::BufWriter<std::fs::File>,
}

impl Client {
    /// Connects and checks that the service speaks the same protocol version.
    pub fn connect() -> io::Result<Self> {
        // All pipe instances can be busy for a moment while the service creates the next one
        let mut attempts = 0;
        let file = loop {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(pipe_name())
            {
                Ok(file) => break file,
                // ERROR_PIPE_BUSY
                Err(e) if e.raw_os_error() == Some(231) && attempts < 50 => {
                    attempts += 1;
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => return Err(e),
            }
        };

        let mut client = Self {
            reader: io::BufReader::new(file.try_clone()?),
            writer: io::BufWriter::new(file),
        };
        match client.request(&Request::Hello {
            version: PROTOCOL_VERSION,
        })? {
            Response::Hello { version } if version == PROTOCOL_VERSION => Ok(client),
            Response::Hello { version } => Err(io::Error::other(format!(
                "The service speaks protocol version {}, expected {}",
                version, PROTOCOL_VERSION
            ))),
            other => Err(io::Error::other(format!("Unexpected response {:?}", other))),
        }
    }

    pub fn request(&mut self, request: &Request) -> io::Result<Response> {
        write_message(&mut self.writer, request)?;
        read_message(&mut self.reader, MAX_RESPONSE_BYTES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut buf = Vec::new();
        let request = Request::Search {
            query: "notepad".into(),
            sort: Sort::default(),
            files: true,
            folders: false,
        };
        write_message(&mut buf, &request).unwrap();
        let decoded: Request = read_message(&mut buf.as_slice(), MAX_REQUEST_BYTES).unwrap();
        assert!(matches!(decoded, Request::Search { query, .. } if query == "notepad"));
    }

    #[test]
    fn rejects_oversized_messages() {
        let mut buf = Vec::new();
        write_message(&mut buf, &"x".repeat(100)).unwrap();
        assert!(read_message::<String>(&mut buf.as_slice(), 10).is_err());
    }
}
