//! Keeps the indices of all volumes loaded and up to date.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use eyre::{bail, Result};
use rayon::prelude::*;

use crate::index::build::{scan_volume, ScanOptions};
use crate::index::persist::load_current;
use crate::index::update::{fetch_update, RecordUpdate};
use crate::index::VolumeIndex;
use crate::ntfs::io::Handle;
use crate::ntfs::usn::{JournalError, JournalReader};
use crate::ntfs::volume::Volume;

pub struct VolumeSlot {
    pub volume: Volume,
    pub index: RwLock<VolumeIndex>,
    pub status: Mutex<String>,
}

pub struct IndexSet {
    pub volumes: Vec<VolumeSlot>,
    /// Bumped on every change to any index
    pub generation: AtomicU64,
}

impl IndexSet {
    pub fn new(volumes: Vec<Volume>) -> Arc<Self> {
        Arc::new(Self {
            volumes: volumes
                .into_iter()
                .map(|volume| VolumeSlot {
                    volume,
                    index: RwLock::new(VolumeIndex::empty(volume)),
                    status: Mutex::new("Waiting".into()),
                })
                .collect(),
            generation: AtomicU64::new(0),
        })
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn bump(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
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

    /// Combined status line of all volumes
    pub fn status(&self) -> String {
        self.volumes
            .iter()
            .map(|v| format!("{}: {}", v.volume.id, v.status.lock().unwrap()))
            .collect::<Vec<_>>()
            .join("   ")
    }

    fn set_status(&self, i: usize, status: String) {
        *self.volumes[i].status.lock().unwrap() = status;
        self.bump();
    }

    fn run_volume(&self, i: usize) {
        let slot = &self.volumes[i];
        let mut try_saved = true;
        loop {
            let t = Instant::now();
            let saved = try_saved.then(|| load_current(slot.volume));
            try_saved = false;

            match saved {
                Some(Ok(index)) => {
                    let files = index.file_count();
                    *slot.index.write().unwrap() = index;
                    self.set_status(i, format!("{} files, loaded in {:.2?}", files, t.elapsed()));
                }
                saved => {
                    if let Some(Err(e)) = saved {
                        eprintln!("Not using saved index of {}: {:#}", slot.volume.id, e);
                    }
                    self.set_status(i, "Indexing...".into());
                    match scan_volume(slot.volume, &ScanOptions::default()) {
                        Ok((index, _)) => {
                            let files = index.file_count();
                            *slot.index.write().unwrap() = index;
                            self.set_status(
                                i,
                                format!("{} files, indexed in {:.2?}", files, t.elapsed()),
                            );
                            // Save right away so the next start is fast even if we crash
                            if let Err(e) = slot.index.read().unwrap().save() {
                                eprintln!("Failed to save index of {}: {:#}", slot.volume.id, e);
                            }
                        }
                        Err(e) => {
                            self.set_status(i, format!("Indexing failed: {:#}", e));
                            return;
                        }
                    }
                }
            }

            match self.follow_journal(i) {
                Err(JournalError::Reset) => {
                    eprintln!("Journal of {}: was reset, rebuilding index", slot.volume.id);
                    continue;
                }
                Err(JournalError::Other(e)) => {
                    self.set_status(i, format!("Live updates stopped: {:#}", e));
                    return;
                }
                Ok(()) => return,
            }
        }
    }

    /// Saves every loaded index. Called on exit.
    pub fn save_all(&self) {
        for slot in &self.volumes {
            let index = slot.index.read().unwrap();
            if index.journal_id == 0 {
                continue;
            }
            if let Err(e) = index.save() {
                eprintln!("Failed to save index of {}: {:#}", slot.volume.id, e);
            }
        }
    }

    fn follow_journal(&self, i: usize) -> Result<(), JournalError> {
        let slot = &self.volumes[i];
        let mut follower = JournalFollower::new(&slot.index.read().unwrap())?;

        loop {
            let changed = follower.wait_for_changes()?;
            let updates = follower.fetch(&changed);
            slot.index
                .write()
                .unwrap()
                .apply_updates(&updates, follower.reader.next_usn());
            self.bump();
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
