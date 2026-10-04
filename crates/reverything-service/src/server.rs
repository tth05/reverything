//! Named pipe server. Every client connection gets its own thread and its own result set.
//!
//! Clients run with fewer privileges than the service, so everything they send is treated as
//! untrusted: message sizes are capped and the service never touches files on their behalf.

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::os::windows::io::FromRawHandle;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use eyre::{bail, Result};
use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, ERROR_PIPE_CONNECTED, HANDLE};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::GetCurrentProcess;

use reverything_core::index::build::ScanStats;
use reverything_core::index::search::{FolderExclusion, Matcher, Query};
use reverything_core::index::{ATTRIBUTE_MASK, FLAG_DIRECTORY};
use reverything_core::search::{exclusions, hit_parts, search_all, Hit};
use reverything_core::service::{self as core_service, BatchStats, IndexSet, SaveStats};
use reverything_protocol::{
    read_message, write_message, BatchTimings, Request, Response, Row, SaveTimings, SavedIndex,
    ScanTimings, Sort, SortColumn, Status, VolumeState, VolumeStatus, MAX_REQUEST_BYTES,
    MAX_ROWS_PER_REQUEST, PROTOCOL_VERSION,
};

use crate::config::Config;
use crate::security::{SecurityAttributes, PIPE_SDDL};

pub struct Server {
    set: Arc<IndexSet>,
    pipe: String,
    searches: AtomicU64,
    /// Duration of the last search in microseconds, `u64::MAX` if there was none
    last_search_us: AtomicU64,
}

impl Server {
    pub fn new(set: Arc<IndexSet>, pipe: String) -> Arc<Self> {
        Arc::new(Self {
            set,
            pipe,
            searches: AtomicU64::new(0),
            last_search_us: AtomicU64::new(u64::MAX),
        })
    }

    /// Accepts clients forever.
    pub fn serve(self: Arc<Self>) -> Result<()> {
        let security = SecurityAttributes::from_sddl(PIPE_SDDL)?;
        let mut first = true;
        loop {
            let pipe = create_instance(&self.pipe, &security, first)?;
            first = false;

            match unsafe { ConnectNamedPipe(pipe, None) } {
                Ok(()) => {}
                // The client connected between creating the instance and waiting for it
                Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => {}
                Err(e) => {
                    log::warn!("Failed to accept a client: {}", e);
                    unsafe {
                        let _ = CloseHandle(pipe);
                    }
                    continue;
                }
            }

            let file = unsafe { File::from_raw_handle(pipe.0) };
            let server = self.clone();
            std::thread::Builder::new()
                .name("client".into())
                .spawn(move || {
                    if let Err(e) = server.handle_client(file) {
                        log::info!("Client disconnected: {:#}", e);
                    }
                })?;
        }
    }

    fn handle_client(&self, file: File) -> Result<()> {
        let mut session = Session::default();
        let result = self.serve_session(&mut session, &file);
        if session.active == Some(true) {
            self.set.client_active(false);
        }
        result
    }

    fn serve_session(&self, session: &mut Session, file: &File) -> Result<()> {
        let mut reader = BufReader::new(file);
        let mut writer = BufWriter::new(file);
        loop {
            let request: Request = match read_message(&mut reader, MAX_REQUEST_BYTES) {
                Ok(request) => request,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                // A closed pipe shows up as ERROR_BROKEN_PIPE
                Err(e) if e.raw_os_error() == Some(109) => return Ok(()),
                Err(e) => return Err(e.into()),
            };
            let response = self.handle(session, request);
            write_message(&mut writer, &response)?;
        }
    }

    fn handle(&self, session: &mut Session, request: Request) -> Response {
        match request {
            Request::Hello { .. } => Response::Hello {
                version: PROTOCOL_VERSION,
            },
            Request::Search {
                query,
                sort,
                files,
                folders,
            } => {
                // Clients that never say whether they are active (the command line) are active
                // while connected
                if session.active.is_none() {
                    session.active = Some(true);
                    self.set.client_active(true);
                }
                // Right after becoming active, the indices may still be loading
                if session.active == Some(true) {
                    self.set.wait_until_awake(WAKE_WAIT);
                }
                let t = Instant::now();
                let mut query = Query::parse(&query);
                session.names = query
                    .include
                    .iter()
                    .filter_map(|t| t.name_text.as_deref())
                    .map(Matcher::new)
                    .collect();
                session.folders = query
                    .include
                    .iter()
                    .flat_map(|t| t.dirs.iter().cloned())
                    .collect();
                query.folders.truncate(MAX_EXCLUDED_FOLDERS);
                query.skip_files = !files;
                query.skip_folders = !folders;
                let hits = {
                    let excluded =
                        session.exclusions(&self.set, std::mem::take(&mut query.folders));
                    search_all(&self.set, &query, core_sort(sort), excluded)
                };
                session.hits = hits;
                session.search += 1;
                let took_us = t.elapsed().as_micros() as u64;
                self.searches.fetch_add(1, Ordering::Relaxed);
                self.last_search_us.store(took_us, Ordering::Relaxed);
                Response::Search {
                    search: session.search,
                    total: session.hits.len() as u64,
                    took_us,
                }
            }
            Request::Rows {
                search,
                start,
                count,
            } => {
                if search != session.search {
                    return Response::Error(format!("Search {} is no longer current", search));
                }
                let start_ix = (start as usize).min(session.hits.len());
                let end = start_ix
                    .saturating_add(count.min(MAX_ROWS_PER_REQUEST) as usize)
                    .min(session.hits.len());
                Response::Rows {
                    search,
                    start,
                    rows: self.rows(&session.hits[start_ix..end], session),
                }
            }
            Request::Status => Response::Status(self.status()),
            Request::RefreshVolumes => {
                self.set.refresh_volumes();
                Response::Status(self.status())
            }
            Request::SetActive { active } => {
                if session.active.unwrap_or(false) != active {
                    self.set.client_active(active);
                }
                session.active = Some(active);
                Response::Done
            }
            Request::SetVolumes { volumes } => {
                // Enabled volumes that are gone (an unplugged disk) stay enabled
                let volumes = volumes
                    .into_iter()
                    .filter(|&c| {
                        IndexSet::slot_of(c).is_some_and(|i| {
                            let slot = &self.set.volumes[i];
                            slot.present() || slot.enabled()
                        })
                    })
                    .collect::<Vec<_>>();
                self.set.set_enabled(&volumes);
                let config = Config {
                    volumes: self.set.enabled(),
                };
                match config.save(self.set.db_dir()) {
                    Ok(()) => Response::Done,
                    Err(e) => Response::Error(format!("Failed to save the settings: {}", e)),
                }
            }
        }
    }

    fn rows(&self, hits: &[Hit], session: &Session) -> Vec<Row> {
        let indices = self
            .set
            .volumes
            .iter()
            .map(|v| v.index.read().unwrap())
            .collect::<Vec<_>>();

        hits.iter()
            .map(|&hit| {
                let (v, id) = hit_parts(hit);
                match indices.get(v) {
                    // Entries can disappear between the search and fetching its rows
                    Some(index) if index.is_in_use(id) => {
                        let flags = index.flags(id);
                        let name = index.name_str(id).to_string();
                        let folder = index.folder_path(id);
                        Row {
                            highlights: highlights(&name, &session.names),
                            folder_highlights: folder_highlights(&folder, &session.folders),
                            name,
                            folder,
                            size: index.size(id),
                            directory: flags & FLAG_DIRECTORY != 0,
                            modified: index.modified(id),
                            created: index.created(id),
                            attributes: flags & ATTRIBUTE_MASK,
                        }
                    }
                    _ => Row {
                        name: String::new(),
                        folder: String::new(),
                        size: 0,
                        directory: false,
                        modified: 0,
                        created: 0,
                        attributes: 0,
                        highlights: Vec::new(),
                        folder_highlights: Vec::new(),
                    },
                }
            })
            .collect()
    }

    fn status(&self) -> Status {
        let mut memory = PROCESS_MEMORY_COUNTERS::default();
        unsafe {
            let _ = GetProcessMemoryInfo(
                GetCurrentProcess(),
                &mut memory,
                size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            );
        }
        let last_search = self.last_search_us.load(Ordering::Relaxed);

        Status {
            service_version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_us: self.set.started.elapsed().as_micros() as u64,
            generation: self.set.generation(),
            working_set_bytes: memory.WorkingSetSize as u64,
            private_bytes: memory.PagefileUsage as u64,
            searches: self.searches.load(Ordering::Relaxed),
            last_search_us: (last_search != u64::MAX).then_some(last_search),
            // Drives that exist, and enabled ones even while they are gone
            volumes: (0..self.set.volumes.len())
                .filter(|&i| self.set.volumes[i].present() || self.set.volumes[i].enabled())
                .map(|i| volume_status(self.set.volumes[i].volume.id, self.set.stats(i)))
                .collect(),
        }
    }
}

/// Matched byte ranges of `name`, sorted and without overlaps.
fn highlights(name: &str, names: &[Matcher]) -> Vec<(u32, u32)> {
    merge(names.iter().flat_map(|m| m.find(name)).collect())
}

/// Byte ranges of the folders in `path` that match the folder parts of the search.
fn folder_highlights(path: &str, folders: &[Matcher]) -> Vec<(u32, u32)> {
    if folders.is_empty() {
        return Vec::new();
    }
    let mut ranges = Vec::new();
    let mut start = 0;
    for part in path.split('\\') {
        for m in folders {
            // Wildcard patterns have to match the whole folder name
            if m.matches(part.as_bytes()) {
                ranges.extend(
                    m.find(part)
                        .into_iter()
                        .map(|r| r.start + start..r.end + start),
                );
            }
        }
        start += part.len() + 1;
    }
    merge(ranges)
}

/// Sorted ranges without overlaps.
fn merge(mut ranges: Vec<std::ops::Range<usize>>) -> Vec<(u32, u32)> {
    ranges.sort_by_key(|r| r.start);
    let mut merged: Vec<(u32, u32)> = Vec::new();
    for r in ranges {
        match merged.last_mut() {
            Some(last) if r.start as u32 <= last.1 => last.1 = last.1.max(r.end as u32),
            _ => merged.push((r.start as u32, r.end as u32)),
        }
    }
    merged
}

/// More are ignored
const MAX_EXCLUDED_FOLDERS: usize = 256;
/// How long a search waits for indices that are still loading after the app became active
const WAKE_WAIT: Duration = Duration::from_secs(3);
/// How long resolved folder exclusions are reused before they are looked up again, so new
/// directories below excluded folders get excluded too
const EXCLUSION_CACHE_TIME: Duration = Duration::from_secs(10);

/// Folder exclusions, when they were resolved, and the bitsets per volume
type CachedExclusions = (Vec<FolderExclusion>, Instant, Vec<Option<Vec<u64>>>);

#[derive(Default)]
struct Session {
    /// Whether the client's window is active, `None` until it says
    active: Option<bool>,
    search: u64,
    hits: Vec<Hit>,
    /// Name parts of the search, to highlight them in the rows
    names: Vec<Matcher>,
    /// Folder parts of the search (`system32\`), to highlight them in the folder column
    folders: Vec<Matcher>,
    /// Folder exclusions of the last query, when they were resolved, and the resulting bitsets
    /// per volume
    exclusions: Option<CachedExclusions>,
}

impl Session {
    /// Resolving the folders walks every directory, so the result is cached while typing.
    fn exclusions(&mut self, set: &IndexSet, folders: Vec<FolderExclusion>) -> &[Option<Vec<u64>>] {
        if folders.is_empty() {
            self.exclusions = None;
            return &[];
        }
        let stale = self.exclusions.as_ref().is_none_or(|(cached, at, _)| {
            *cached != folders || at.elapsed() > EXCLUSION_CACHE_TIME
        });
        if stale {
            let resolved = exclusions(set, &folders);
            self.exclusions = Some((folders, Instant::now(), resolved));
        }
        &self.exclusions.as_ref().unwrap().2
    }
}

fn create_instance(name: &str, security: &SecurityAttributes, first: bool) -> Result<HANDLE> {
    let mut open_mode = PIPE_ACCESS_DUPLEX;
    if first {
        // Fails if someone else already owns the name, so clients can not be tricked into
        // talking to an impostor
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    let pipe = unsafe {
        CreateNamedPipeW(
            &HSTRING::from(name),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            64 * 1024,
            64 * 1024,
            0,
            Some(security.as_ptr()),
        )
    };
    if pipe.is_invalid() {
        let e = std::io::Error::last_os_error();
        if first {
            bail!(
                "Failed to create {} (is another instance running?): {}",
                name,
                e
            );
        }
        bail!("Failed to create a pipe instance: {}", e);
    }
    Ok(pipe)
}

fn core_sort(sort: Sort) -> reverything_core::search::Sort {
    use reverything_core::search::SortColumn as Core;
    reverything_core::search::Sort {
        column: match sort.column {
            SortColumn::Relevance => Core::Relevance,
            SortColumn::Name => Core::Name,
            SortColumn::Path => Core::Path,
            SortColumn::Size => Core::Size,
            SortColumn::Modified => Core::Modified,
            SortColumn::Created => Core::Created,
            SortColumn::Attributes => Core::Attributes,
        },
        ascending: sort.ascending,
    }
}

fn us(d: std::time::Duration) -> u64 {
    d.as_micros() as u64
}

fn unix(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn volume_status(letter: char, s: core_service::VolumeStats) -> VolumeStatus {
    let batch = |b: BatchStats| BatchTimings {
        records: b.records as u64,
        updates: b.updates as u64,
        fetch_us: us(b.fetch),
        apply_us: us(b.apply),
        at: unix(b.at),
    };
    let scan = |s: ScanStats| ScanTimings {
        open_us: us(s.open),
        read_parse_us: us(s.read_parse),
        merge_us: us(s.merge),
        sort_us: us(s.sort),
        folder_sizes_us: us(s.folder_sizes),
        bytes_read: s.bytes_read,
        records: s.used_records,
        links: s.links as u64,
    };
    let save = |s: SaveStats| SaveTimings {
        took_us: us(s.took),
        bytes: s.bytes,
        at: unix(s.at),
    };

    VolumeStatus {
        letter,
        state: match s.state {
            core_service::VolumeState::Disabled => VolumeState::Disabled,
            core_service::VolumeState::Waiting => VolumeState::Waiting,
            core_service::VolumeState::Loading => VolumeState::Loading,
            core_service::VolumeState::Indexing => VolumeState::Indexing,
            core_service::VolumeState::Ready => VolumeState::Ready,
            core_service::VolumeState::Asleep => VolumeState::Asleep,
            core_service::VolumeState::Offline => VolumeState::Offline,
            core_service::VolumeState::Failed(e) => VolumeState::Failed(e),
        },
        entries: s.entries as u64,
        index_bytes: s.index_bytes as u64,
        load_us: s.load.map(us),
        saved_index: match s.saved_index {
            core_service::SavedIndex::NotChecked => SavedIndex::NotChecked,
            core_service::SavedIndex::Missing => SavedIndex::Missing,
            core_service::SavedIndex::Loaded => SavedIndex::Loaded,
            core_service::SavedIndex::Discarded(e) => SavedIndex::Discarded(e),
        },
        scan: s.scan.map(scan),
        catch_up: s.catch_up.map(batch),
        last_batch: s.last_batch.map(batch),
        batches: s.batches,
        records_updated: s.records_updated,
        last_save: s.last_save.map(save),
    }
}
