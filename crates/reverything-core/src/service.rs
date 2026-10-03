//! Keeps the indices of all volumes loaded and up to date, and records how long everything took.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

use eyre::{bail, Result};
use rayon::prelude::*;

use crate::index::build::{scan_volume, ScanOptions, ScanStats};
use crate::index::persist::{load_current, load_offline};
use crate::index::update::{fetch_update, RecordUpdate};
use crate::index::VolumeIndex;
use crate::ntfs::io::Handle;
use crate::ntfs::usn::{JournalError, JournalReader};
use crate::ntfs::volume::Volume;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum VolumeState {
    #[default]
    Waiting,
    Loading,
    Indexing,
    /// Searchable and kept up to date
    Ready,
    /// Searchable, but loaded without volume access and not updated
    Offline,
    Failed(String),
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
    /// Time from the start of the service until the volume was searchable
    pub ready_after: Option<Duration>,
    /// Loading the saved index
    pub load: Option<Duration>,
    /// Why the saved index could not be used
    pub load_error: Option<String>,
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
}

pub struct IndexSet {
    pub volumes: Vec<VolumeSlot>,
    /// Bumped on every change to any index
    pub generation: AtomicU64,
    pub started: Instant,
    db_dir: PathBuf,
}

impl IndexSet {
    /// `db_dir` is where indices are saved and loaded from.
    pub fn new(volumes: Vec<Volume>, db_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            volumes: volumes
                .into_iter()
                .map(|volume| VolumeSlot {
                    volume,
                    index: RwLock::new(VolumeIndex::empty(volume)),
                    stats: Mutex::new(VolumeStats::default()),
                    dirty: AtomicBool::new(false),
                })
                .collect(),
            generation: AtomicU64::new(0),
            started: Instant::now(),
            db_dir,
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

    fn update_stats(&self, i: usize, f: impl FnOnce(&mut VolumeStats)) {
        f(&mut self.volumes[i].stats.lock().unwrap());
        self.bump();
    }

    /// Short summary of all volumes
    pub fn status(&self) -> String {
        (0..self.volumes.len())
            .map(|i| {
                let s = self.stats(i);
                let state = match &s.state {
                    VolumeState::Ready | VolumeState::Offline => format!("{} files", s.entries),
                    VolumeState::Failed(e) => format!("failed: {}", e),
                    state => format!("{:?}", state),
                };
                format!("{}: {}", self.volumes[i].volume.id, state)
            })
            .collect::<Vec<_>>()
            .join("   ")
    }

    /// Loads every volume in the background and keeps it updated from the journal.
    pub fn start(self: &Arc<Self>) {
        for i in 0..self.volumes.len() {
            let set = self.clone();
            std::thread::Builder::new()
                .name(format!("volume {}", self.volumes[i].volume.id))
                .spawn(move || set.run_volume(i))
                .expect("Failed to spawn volume thread");
        }
    }

    /// Loads the saved indices without volume access. They are not kept up to date.
    pub fn load_offline(&self) {
        for (i, slot) in self.volumes.iter().enumerate() {
            let t = Instant::now();
            match load_offline(slot.volume, &self.db_dir) {
                Ok(index) => {
                    self.install(i, index, VolumeState::Offline);
                    self.update_stats(i, |s| s.load = Some(t.elapsed()));
                }
                Err(e) => {
                    self.update_stats(i, |s| s.state = VolumeState::Failed(format!("{:#}", e)))
                }
            }
        }
    }

    fn install(&self, i: usize, index: VolumeIndex, state: VolumeState) {
        let (entries, bytes) = (index.file_count(), index.heap_bytes());
        *self.volumes[i].index.write().unwrap() = index;
        let ready_after = self.started.elapsed();
        self.update_stats(i, |s| {
            s.state = state;
            s.entries = entries;
            s.index_bytes = bytes;
            s.ready_after.get_or_insert(ready_after);
        });
    }

    fn run_volume(&self, i: usize) {
        let slot = &self.volumes[i];
        let mut try_saved = true;
        loop {
            if try_saved {
                try_saved = false;
                self.update_stats(i, |s| s.state = VolumeState::Loading);
                let t = Instant::now();
                match load_current(slot.volume, &self.db_dir) {
                    Ok(index) => {
                        self.install(i, index, VolumeState::Ready);
                        self.update_stats(i, |s| s.load = Some(t.elapsed()));
                    }
                    Err(e) => {
                        log::info!("Not using saved index of {}: {:#}", slot.volume.id, e);
                        self.update_stats(i, |s| s.load_error = Some(format!("{:#}", e)));
                    }
                }
            }

            if self.stats(i).state != VolumeState::Ready {
                self.update_stats(i, |s| s.state = VolumeState::Indexing);
                match scan_volume(slot.volume, &ScanOptions::default()) {
                    Ok((index, scan)) => {
                        log::info!("Indexed {} in {:?}", slot.volume.id, scan.total());
                        self.install(i, index, VolumeState::Ready);
                        self.update_stats(i, |s| s.scan = Some(scan));
                        // Save right away so the next start is fast even if we crash
                        slot.dirty.store(true, Ordering::Release);
                        self.save(i);
                    }
                    Err(e) => {
                        log::error!("Indexing {} failed: {:#}", slot.volume.id, e);
                        self.update_stats(i, |s| s.state = VolumeState::Failed(format!("{:#}", e)));
                        return;
                    }
                }
            }

            match self.follow_journal(i) {
                Err(JournalError::Reset) => {
                    log::warn!(
                        "Journal of {} was reset, rebuilding the index",
                        slot.volume.id
                    );
                    self.update_stats(i, |s| s.state = VolumeState::Indexing);
                    continue;
                }
                Err(JournalError::Other(e)) => {
                    log::error!("Live updates of {} stopped: {:#}", slot.volume.id, e);
                    self.update_stats(i, |s| {
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
        if !slot.dirty.swap(false, Ordering::AcqRel) {
            return;
        }
        let t = Instant::now();
        let result = slot.index.read().unwrap().save(&self.db_dir);
        match result {
            Ok(bytes) => self.update_stats(i, |s| {
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

    fn follow_journal(&self, i: usize) -> Result<(), JournalError> {
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
            self.update_stats(i, |s| {
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
