//! Keeps the indices of the enabled volumes loaded and up to date, and records how long
//! everything took.

use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use eyre::{bail, Result};
use rayon::prelude::*;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::IO::CancelSynchronousIo;

use crate::index::build::{scan_volume, ScanOptions, ScanStats};
use crate::index::persist::{db_path, load_current, load_offline};
use crate::index::update::{fetch_update, RecordUpdate};
use crate::index::VolumeIndex;
use crate::ntfs::io::Handle;
use crate::ntfs::usn::{JournalError, JournalReader};
use crate::ntfs::volume::Volume;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum VolumeState {
    /// Not indexed
    #[default]
    Disabled,
    Waiting,
    Loading,
    Indexing,
    /// Searchable and kept up to date
    Ready,
    /// Searchable, but loaded without volume access and not updated
    Offline,
    Failed(String),
}

/// What happened to the saved index when the volume was enabled.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SavedIndex {
    #[default]
    NotChecked,
    /// There was none, so the volume was scanned
    Missing,
    Loaded,
    /// It could not be used, so the volume was scanned
    Discarded(String),
}

/// One batch of journal changes applied to an index.
#[derive(Debug, Clone, Copy)]
pub struct BatchStats {
    pub records: usize,
    pub updates: usize,
    pub fetch: Duration,
    pub apply: Duration,
    pub at: SystemTime,
}

#[derive(Debug, Clone, Copy)]
pub struct SaveStats {
    pub took: Duration,
    pub bytes: u64,
    pub at: SystemTime,
}

#[derive(Debug, Clone, Default)]
pub struct VolumeStats {
    pub state: VolumeState,
    pub entries: usize,
    pub index_bytes: usize,
    /// Loading the saved index
    pub load: Option<Duration>,
    pub saved_index: SavedIndex,
    /// Full scan of the MFT
    pub scan: Option<ScanStats>,
    /// The first journal batch, which catches up with the changes since the index was saved or
    /// scanned
    pub catch_up: Option<BatchStats>,
    pub last_batch: Option<BatchStats>,
    pub batches: u64,
    pub records_updated: u64,
    pub last_save: Option<SaveStats>,
}

pub struct VolumeSlot {
    pub volume: Volume,
    pub index: RwLock<VolumeIndex>,
    pub stats: Mutex<VolumeStats>,
    /// Changed since it was last saved
    dirty: AtomicBool,
    enabled: AtomicBool,
    /// Bumped whenever the volume is enabled or disabled. Threads working for an earlier run
    /// stop and leave the index and stats alone.
    run: AtomicU64,
    /// Thread of the current run
    thread: Mutex<Option<JoinHandle<()>>>,
    /// Held while the saved index is written or deleted
    file: Mutex<()>,
}

impl VolumeSlot {
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }
}

pub struct IndexSet {
    pub volumes: Vec<VolumeSlot>,
    /// Bumped on every change to any index
    pub generation: AtomicU64,
    pub started: Instant,
    db_dir: PathBuf,
    /// Load saved indices without volume access and do not update them
    offline: bool,
}

impl IndexSet {
    /// All volumes start disabled, see [`IndexSet::set_enabled`]. `db_dir` is where indices
    /// are saved and loaded from.
    pub fn new(volumes: Vec<Volume>, db_dir: PathBuf) -> Arc<Self> {
        Self::create(volumes, db_dir, false)
    }

    /// Like [`IndexSet::new`], but enabled volumes load their saved index without volume
    /// access, which needs no admin rights. They are not kept up to date.
    pub fn new_offline(volumes: Vec<Volume>, db_dir: PathBuf) -> Arc<Self> {
        Self::create(volumes, db_dir, true)
    }

    fn create(volumes: Vec<Volume>, db_dir: PathBuf, offline: bool) -> Arc<Self> {
        Arc::new(Self {
            volumes: volumes
                .into_iter()
                .map(|volume| VolumeSlot {
                    volume,
                    index: RwLock::new(VolumeIndex::empty(volume)),
                    stats: Mutex::new(VolumeStats::default()),
                    dirty: AtomicBool::new(false),
                    enabled: AtomicBool::new(false),
                    run: AtomicU64::new(0),
                    thread: Mutex::new(None),
                    file: Mutex::new(()),
                })
                .collect(),
            generation: AtomicU64::new(0),
            started: Instant::now(),
            db_dir,
            offline,
        })
    }

    pub fn db_dir(&self) -> &Path {
        &self.db_dir
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn bump(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    pub fn stats(&self, i: usize) -> VolumeStats {
        self.volumes[i].stats.lock().unwrap().clone()
    }

    fn is_current(&self, i: usize, run: u64) -> bool {
        self.volumes[i].run.load(Ordering::Acquire) == run
    }

    /// Changes the stats of volume `i` if `run` is still its current run.
    fn update_stats(&self, i: usize, run: u64, f: impl FnOnce(&mut VolumeStats)) {
        let mut stats = self.volumes[i].stats.lock().unwrap();
        if self.is_current(i, run) {
            f(&mut stats);
            drop(stats);
            self.bump();
        }
    }

    /// Short summary of all volumes
    pub fn status(&self) -> String {
        (0..self.volumes.len())
            .map(|i| {
                let s = self.stats(i);
                let state = match &s.state {
                    VolumeState::Ready | VolumeState::Offline => format!("{} files", s.entries),
                    VolumeState::Disabled => "not indexed".to_string(),
                    VolumeState::Failed(e) => format!("failed: {}", e),
                    state => format!("{:?}", state),
                };
                format!("{}: {}", self.volumes[i].volume.id, state)
            })
            .collect::<Vec<_>>()
            .join("   ")
    }

    /// Letters of the enabled volumes
    pub fn enabled(&self) -> Vec<char> {
        self.volumes
            .iter()
            .filter(|v| v.enabled())
            .map(|v| v.volume.id)
            .collect()
    }

    /// Indexes the volumes in `letters` in the background and keeps them updated from the
    /// journal. Stops indexing the others, which drops their index and deletes the saved one.
    pub fn set_enabled(self: &Arc<Self>, letters: &[char]) {
        for (i, slot) in self.volumes.iter().enumerate() {
            let enable = letters.contains(&slot.volume.id);
            if enable == slot.enabled() {
                continue;
            }
            let mut thread = slot.thread.lock().unwrap();
            slot.enabled.store(enable, Ordering::Release);
            let run = slot.run.fetch_add(1, Ordering::AcqRel) + 1;

            if enable {
                log::info!("Indexing {}:", slot.volume.id);
                *slot.stats.lock().unwrap() = VolumeStats {
                    state: VolumeState::Waiting,
                    ..Default::default()
                };
                let set = self.clone();
                // A thread of an earlier run may still be stopping, it notices that it is
                // outdated on its own
                *thread = Some(
                    std::thread::Builder::new()
                        .name(format!("volume {}", slot.volume.id))
                        .spawn(move || set.run_volume(i, run))
                        .expect("Failed to spawn volume thread"),
                );
            } else {
                log::info!("No longer indexing {}:", slot.volume.id);
                *slot.index.write().unwrap() = VolumeIndex::empty(slot.volume);
                *slot.stats.lock().unwrap() = VolumeStats::default();
                slot.dirty.store(false, Ordering::Release);
                let previous = thread.take();
                let set = self.clone();
                std::thread::Builder::new()
                    .name(format!("stopping {}", slot.volume.id))
                    .spawn(move || set.stop_volume(i, previous))
                    .expect("Failed to spawn thread");
            }
            self.bump();
        }
    }

    /// Deletes the saved indices of the volumes that are not enabled, e.g. left over from
    /// before the user turned a volume off.
    pub fn delete_unused(&self) {
        for slot in self.volumes.iter().filter(|v| !v.enabled()) {
            let _file = slot.file.lock().unwrap();
            let path = db_path(&self.db_dir, slot.volume);
            if std::fs::remove_file(&path).is_ok() {
                log::info!("Deleted {}", path.display());
            }
        }
    }

    /// Waits for the thread of a disabled volume to stop, then deletes the saved index.
    fn stop_volume(&self, i: usize, thread: Option<JoinHandle<()>>) {
        let slot = &self.volumes[i];
        if let Some(thread) = thread {
            // It may be blocked waiting for journal entries of a quiet volume
            while !thread.is_finished() {
                unsafe {
                    let _ = CancelSynchronousIo(HANDLE(thread.as_raw_handle()));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = thread.join();
        }
        let _file = slot.file.lock().unwrap();
        if !slot.enabled() && !self.offline {
            let path = db_path(&self.db_dir, slot.volume);
            match std::fs::remove_file(&path) {
                Ok(()) => log::info!("Deleted {}", path.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => log::warn!("Failed to delete {}: {}", path.display(), e),
            }
        }
    }

    /// Replaces the index of volume `i` if `run` is still its current run.
    fn install(&self, i: usize, run: u64, index: VolumeIndex, state: VolumeState) -> bool {
        let (entries, bytes) = (index.file_count(), index.heap_bytes());
        {
            let mut current = self.volumes[i].index.write().unwrap();
            if !self.is_current(i, run) {
                return false;
            }
            *current = index;
        }
        self.update_stats(i, run, |s| {
            s.state = state;
            s.entries = entries;
            s.index_bytes = bytes;
        });
        true
    }

    /// Loads or builds the index of volume `i` and keeps it up to date until the volume is
    /// disabled.
    fn run_volume(&self, i: usize, run: u64) {
        let slot = &self.volumes[i];
        if self.offline {
            let t = Instant::now();
            match load_offline(slot.volume, &self.db_dir) {
                Ok(index) => {
                    self.install(i, run, index, VolumeState::Offline);
                    self.update_stats(i, run, |s| {
                        s.load = Some(t.elapsed());
                        s.saved_index = SavedIndex::Loaded;
                    });
                }
                Err(e) => self.update_stats(i, run, |s| {
                    s.state = VolumeState::Failed(format!("{:#}", e))
                }),
            }
            return;
        }

        let mut ready = false;
        self.update_stats(i, run, |s| s.state = VolumeState::Loading);
        let t = Instant::now();
        if !db_path(&self.db_dir, slot.volume).exists() {
            self.update_stats(i, run, |s| s.saved_index = SavedIndex::Missing);
        } else {
            match load_current(slot.volume, &self.db_dir) {
                Ok(index) => {
                    ready = self.install(i, run, index, VolumeState::Ready);
                    self.update_stats(i, run, |s| {
                        s.load = Some(t.elapsed());
                        s.saved_index = SavedIndex::Loaded;
                    });
                }
                Err(e) => {
                    log::info!("Not using saved index of {}: {:#}", slot.volume.id, e);
                    self.update_stats(i, run, |s| {
                        s.saved_index = SavedIndex::Discarded(format!("{:#}", e))
                    });
                }
            }
        }

        loop {
            if !self.is_current(i, run) {
                return;
            }
            if !ready {
                self.update_stats(i, run, |s| s.state = VolumeState::Indexing);
                match scan_volume(slot.volume, &ScanOptions::default()) {
                    Ok((index, scan)) => {
                        log::info!("Indexed {} in {:?}", slot.volume.id, scan.total());
                        if !self.install(i, run, index, VolumeState::Ready) {
                            return;
                        }
                        self.update_stats(i, run, |s| s.scan = Some(scan));
                        // Save right away so the next start is fast even if we crash
                        slot.dirty.store(true, Ordering::Release);
                        self.save(i);
                    }
                    Err(e) => {
                        log::error!("Indexing {} failed: {:#}", slot.volume.id, e);
                        self.update_stats(i, run, |s| {
                            s.state = VolumeState::Failed(format!("{:#}", e))
                        });
                        return;
                    }
                }
            }

            let result = self.follow_journal(i, run);
            if !self.is_current(i, run) {
                // Disabled, which also cancels waiting for the journal
                return;
            }
            match result {
                Err(JournalError::Reset) => {
                    log::warn!(
                        "Journal of {} was reset, rebuilding the index",
                        slot.volume.id
                    );
                    ready = false;
                }
                Err(JournalError::Other(e)) => {
                    log::error!("Live updates of {} stopped: {:#}", slot.volume.id, e);
                    self.update_stats(i, run, |s| {
                        s.state = VolumeState::Failed(format!("Live updates stopped: {:#}", e))
                    });
                    return;
                }
                Ok(()) => return,
            }
        }
    }

    /// Saves the index of volume `i` if it changed since it was last saved.
    fn save(&self, i: usize) {
        let slot = &self.volumes[i];
        let _file = slot.file.lock().unwrap();
        let run = slot.run.load(Ordering::Acquire);
        if !slot.enabled() || !slot.dirty.swap(false, Ordering::AcqRel) {
            return;
        }
        let t = Instant::now();
        let result = slot.index.read().unwrap().save(&self.db_dir);
        match result {
            Ok(bytes) => self.update_stats(i, run, |s| {
                s.last_save = Some(SaveStats {
                    took: t.elapsed(),
                    bytes,
                    at: SystemTime::now(),
                })
            }),
            Err(e) => {
                slot.dirty.store(true, Ordering::Release);
                log::error!("Failed to save index of {}: {:#}", slot.volume.id, e);
            }
        }
    }

    /// Saves every index that changed since it was last saved.
    pub fn save_changed(&self) {
        for i in 0..self.volumes.len() {
            let ready = self.stats(i).state == VolumeState::Ready;
            if ready {
                self.save(i);
            }
        }
    }

    /// Applies journal changes to volume `i` until `run` is outdated or reading the journal
    /// fails.
    fn follow_journal(&self, i: usize, run: u64) -> Result<(), JournalError> {
        let slot = &self.volumes[i];
        let mut follower = JournalFollower::new(&slot.index.read().unwrap())?;

        loop {
            let changed = follower.wait_for_changes()?;
            let t = Instant::now();
            let updates = follower.fetch(&changed);
            let fetch = t.elapsed();

            let t = Instant::now();
            let (entries, bytes) = {
                let mut index = slot.index.write().unwrap();
                if !self.is_current(i, run) {
                    return Ok(());
                }
                index.apply_updates(&updates, follower.reader.next_usn());
                (index.file_count(), index.heap_bytes())
            };
            slot.dirty.store(true, Ordering::Release);

            let batch = BatchStats {
                records: changed.len(),
                updates: updates.len(),
                fetch,
                apply: t.elapsed(),
                at: SystemTime::now(),
            };
            self.update_stats(i, run, |s| {
                s.entries = entries;
                s.index_bytes = bytes;
                s.catch_up.get_or_insert(batch);
                s.last_batch = Some(batch);
                s.batches += 1;
                s.records_updated += updates.len() as u64;
            });
        }
    }
}

/// Reads the journal from the index' position and fetches the changed records.
pub struct JournalFollower {
    pub reader: JournalReader,
    /// Overlapped handle for fetching records while the reader blocks on its own handle
    handle: Handle,
    record_size: usize,
}

impl JournalFollower {
    pub fn new(index: &VolumeIndex) -> Result<Self> {
        if index.journal_id == 0 {
            bail!("The change journal is not active on {}:", index.volume.id);
        }
        Ok(Self {
            reader: JournalReader::open(index.volume, index.journal_id, index.next_usn)?,
            handle: index.volume.open(true, false)?,
            record_size: index.record_size as usize,
        })
    }

    /// Blocks until something changed, then lets a burst of changes accumulate briefly so large
    /// operations are applied in few batches.
    pub fn wait_for_changes(&mut self) -> Result<Vec<u32>, JournalError> {
        let mut changed = Vec::new();
        while changed.is_empty() {
            self.reader.read_changes(true, &mut changed)?;
        }
        std::thread::sleep(Duration::from_millis(50));
        self.reader.read_changes(false, &mut changed)?;
        Ok(changed)
    }

    /// Changes that are already in the journal, without waiting.
    pub fn poll_changes(&mut self) -> Result<Vec<u32>, JournalError> {
        let mut changed = Vec::new();
        self.reader.read_changes(false, &mut changed)?;
        Ok(changed)
    }

    /// Fetches the current state of the changed records, keeping the order in which they first
    /// appeared in the journal.
    pub fn fetch(&self, changed: &[u32]) -> Vec<RecordUpdate> {
        let mut seen = std::collections::HashSet::with_capacity(changed.len());
        let records = changed
            .iter()
            .copied()
            .filter(|r| seen.insert(*r))
            .collect::<Vec<_>>();
        records
            .par_iter()
            .with_min_len(64)
            .filter_map(|&r| fetch_update(&self.handle, r, self.record_size))
            .collect()
    }
}
