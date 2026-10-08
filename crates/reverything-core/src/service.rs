//! Keeps the indices of the enabled volumes and records how long everything took.
//!
//! The service works for the app and stays out of the way otherwise:
//!
//! - While a client is active (the app window has the focus), every enabled volume is loaded and
//!   follows the change journal live.
//! - Without an active client nothing is applied. The journal is only checked every few minutes
//!   and read in the background when it would otherwise wrap, to remember which records
//!   changed. Becoming active again fetches just those records.
//! - After an hour without an active client the index is dropped from memory. Waking up loads
//!   the saved index and fetches every record that changed since it was saved.
//!
//! Indices are only saved after a full scan and when the service stops, never while running.

use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use eyre::{bail, ensure, Result};
use rayon::prelude::*;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN, THREAD_MODE_BACKGROUND_END,
};
use windows::Win32::System::IO::CancelSynchronousIo;

use crate::index::build::{scan_volume, ScanOptions, ScanStats};
use crate::index::persist::{db_path, load_offline, load_saved, read_header};
use crate::index::update::{fetch_update, RecordUpdate};
use crate::index::VolumeIndex;
use crate::ntfs::io::Handle;
use crate::ntfs::usn::{query_journal, JournalError, JournalReader};
use crate::ntfs::volume::{ntfs_volumes, volume_data, Volume};

/// How often an inactive volume checks how full the journal is
const IDLE_CHECK: Duration = Duration::from_secs(5 * 60);
/// The journal is read once this fraction of it is new, long before it wraps
const READ_AT_FRACTION: u64 = 4;
/// Without an active client for this long, the index is dropped from memory
const UNLOAD_AFTER: Duration = Duration::from_secs(60 * 60);
/// With more changed records than this, a full scan is about as fast as fetching them
const MAX_PENDING: usize = 1_000_000;
/// A failed volume (e.g. an unplugged USB disk) is tried again after this, doubling up to
/// [`RETRY_MAX`]. A client becoming active retries right away.
const RETRY_FIRST: Duration = Duration::from_secs(30);
const RETRY_MAX: Duration = Duration::from_secs(10 * 60);

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
    /// Not in memory while the app is not used, loaded again when it is
    Asleep,
    /// Searchable, but loaded without volume access and not updated
    Offline,
    Failed(String),
}

/// What happened to the saved index.
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
    /// The last catch-up when the app became active, with the changes since it was inactive
    pub catch_up: Option<BatchStats>,
    pub last_batch: Option<BatchStats>,
    pub batches: u64,
    pub records_updated: u64,
    pub last_save: Option<SaveStats>,
}

pub struct VolumeSlot {
    pub volume: Volume,
    /// The drive letter currently holds a fixed NTFS volume, see [`IndexSet::refresh_volumes`]
    present: AtomicBool,
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
    /// The thread is (about to be) blocked waiting for journal records
    blocking: AtomicBool,
    /// Held while the saved index is written or deleted
    file: Mutex<()>,
}

impl VolumeSlot {
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub fn present(&self) -> bool {
        self.present.load(Ordering::Acquire)
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
    /// Clients whose window is active
    active: AtomicUsize,
    /// Counts the times a client became active, so waiting threads notice even a short one
    activations: AtomicU64,
    stopping: AtomicBool,
    /// Wakes volume threads waiting while inactive
    wake_lock: Mutex<()>,
    wake: Condvar,
    /// Signalled on every stats change, for waiting until volumes are ready
    changed_lock: Mutex<()>,
    changed: Condvar,
    /// Called after large amounts of memory were freed (an index dropped or replaced, scan
    /// buffers), to give it back to the system
    trim: OnceLock<Box<dyn Fn() + Send + Sync>>,
    /// [`IDLE_CHECK`] and [`UNLOAD_AFTER`], shorter for tests
    idle_check: Mutex<Duration>,
    unload_after: Mutex<Duration>,
}

impl IndexSet {
    /// There is a slot for every drive letter, so volumes that appear later (a new disk, an
    /// unlocked BitLocker drive, a changed letter) fit in. All start disabled, see
    /// [`IndexSet::set_enabled`], and not present, see [`IndexSet::refresh_volumes`]. `db_dir` is
    /// where indices are saved and loaded from.
    pub fn new(db_dir: PathBuf) -> Arc<Self> {
        Self::create(db_dir, false)
    }

    /// Like [`IndexSet::new`], but enabled volumes load their saved index without volume
    /// access, which needs no admin rights. They are not kept up to date.
    pub fn new_offline(db_dir: PathBuf) -> Arc<Self> {
        Self::create(db_dir, true)
    }

    /// Slot of the volume with this drive letter.
    pub fn slot_of(letter: char) -> Option<usize> {
        letter
            .is_ascii_uppercase()
            .then(|| (letter as u8 - b'A') as usize)
    }

    fn create(db_dir: PathBuf, offline: bool) -> Arc<Self> {
        Arc::new(Self {
            volumes: ('A'..='Z')
                .map(|id| Volume { id })
                .map(|volume| VolumeSlot {
                    volume,
                    present: AtomicBool::new(false),
                    index: RwLock::new(VolumeIndex::empty(volume)),
                    stats: Mutex::new(VolumeStats::default()),
                    dirty: AtomicBool::new(false),
                    enabled: AtomicBool::new(false),
                    run: AtomicU64::new(0),
                    thread: Mutex::new(None),
                    blocking: AtomicBool::new(false),
                    file: Mutex::new(()),
                })
                .collect(),
            generation: AtomicU64::new(0),
            started: Instant::now(),
            db_dir,
            offline,
            active: AtomicUsize::new(0),
            activations: AtomicU64::new(0),
            stopping: AtomicBool::new(false),
            wake_lock: Mutex::new(()),
            wake: Condvar::new(),
            changed_lock: Mutex::new(()),
            changed: Condvar::new(),
            trim: OnceLock::new(),
            idle_check: Mutex::new(IDLE_CHECK),
            unload_after: Mutex::new(UNLOAD_AFTER),
        })
    }

    pub fn db_dir(&self) -> &Path {
        &self.db_dir
    }

    /// Sets what to call after large amounts of memory were freed, to return it to the system.
    pub fn on_trim(&self, f: impl Fn() + Send + Sync + 'static) {
        let _ = self.trim.set(Box::new(f));
    }

    fn trim(&self) {
        if let Some(f) = self.trim.get() {
            f();
        }
    }

    /// Shorter idle timings, for testing.
    pub fn set_idle_timing(&self, check: Duration, unload_after: Duration) {
        *self.idle_check.lock().unwrap() = check;
        *self.unload_after.lock().unwrap() = unload_after;
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn bump(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        let _lock = self.changed_lock.lock().unwrap();
        self.changed.notify_all();
    }

    pub fn stats(&self, i: usize) -> VolumeStats {
        self.volumes[i].stats.lock().unwrap().clone()
    }

    fn is_current(&self, i: usize, run: u64) -> bool {
        self.volumes[i].run.load(Ordering::Acquire) == run
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst) > 0
    }

    fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// Wakes the volume threads waiting while inactive.
    fn notify(&self) {
        let _lock = self.wake_lock.lock().unwrap();
        self.wake.notify_all();
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

    /// Looks for fixed NTFS volumes again. Returns whether anything changed.
    pub fn refresh_volumes(&self) -> bool {
        let found = ntfs_volumes();
        let mut changed = false;
        for slot in &self.volumes {
            let present = found.contains(&slot.volume);
            if slot.present.swap(present, Ordering::AcqRel) != present {
                log::info!(
                    "Volume {}: {}",
                    slot.volume.id,
                    if present { "found" } else { "gone" }
                );
                changed = true;
            }
        }
        if changed {
            self.bump();
        }
        changed
    }

    /// Letters of the enabled volumes
    pub fn enabled(&self) -> Vec<char> {
        self.volumes
            .iter()
            .filter(|v| v.enabled())
            .map(|v| v.volume.id)
            .collect()
    }

    /// A client became active (its window got the focus) or inactive. Volumes are loaded and
    /// follow the journal live while at least one client is active.
    pub fn client_active(self: &Arc<Self>, active: bool) {
        if active {
            if self.active.fetch_add(1, Ordering::SeqCst) == 0 {
                log::info!("A client is active");
                // Cheap, and the app is about to be used: pick up drives that appeared
                self.refresh_volumes();
                self.activations.fetch_add(1, Ordering::SeqCst);
                self.notify();
            }
        } else if self.active.fetch_sub(1, Ordering::SeqCst) == 1 {
            log::info!("No client is active");
            let set = self.clone();
            std::thread::Builder::new()
                .name("pausing".into())
                .spawn(move || set.cancel_journal_waits(|set| set.is_active()))
                .expect("Failed to spawn thread");
        }
    }

    /// Volume threads blocked waiting for journal records only notice a state change with the
    /// next record, which can take long on a quiet volume. This cancels their waits until
    /// `done` returns true or none waits anymore.
    fn cancel_journal_waits(&self, done: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !done(self) {
            let mut waiting = false;
            for slot in &self.volumes {
                if slot.blocking.load(Ordering::SeqCst) {
                    waiting = true;
                    if let Some(thread) = slot.thread.lock().unwrap().as_ref() {
                        unsafe {
                            let _ = CancelSynchronousIo(HANDLE(thread.as_raw_handle()));
                        }
                    }
                }
            }
            if !waiting {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits up to `timeout` until no enabled volume is waking up, so a search right after
    /// becoming active sees the whole index.
    pub fn wait_until_awake(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut lock = self.changed_lock.lock().unwrap();
        loop {
            let waking = self.volumes.iter().any(|slot| {
                slot.enabled()
                    && matches!(
                        slot.stats.lock().unwrap().state,
                        VolumeState::Waiting
                            | VolumeState::Loading
                            | VolumeState::Indexing
                            | VolumeState::Asleep
                    )
            });
            let now = Instant::now();
            if !waking || now >= deadline {
                return;
            }
            lock = self.changed.wait_timeout(lock, deadline - now).unwrap().0;
        }
    }

    /// Brings every index up to date, saves the changed ones and stops the volume threads.
    pub fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.notify();
        self.cancel_journal_waits(|_| false);
        for slot in &self.volumes {
            let thread = slot.thread.lock().unwrap().take();
            if let Some(thread) = thread {
                let _ = thread.join();
            }
        }
    }

    /// Indexes the volumes in `letters` and stops indexing the others, which drops their index
    /// and deletes the saved one.
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
        self.notify();
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
            // It may be waiting while inactive, or blocked waiting for journal records
            while !thread.is_finished() {
                self.notify();
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

    /// Applies `updates` to the installed index of volume `i` with the write lock held as
    /// briefly as possible: names and records grow before, and the names are rewritten once
    /// enough of them are garbage after, both with read access while searches go on. Only the
    /// volume's own thread changes its index, so nothing changes in between. Returns how long
    /// applying took, the entries and the index size, or `None` if `run` is no longer current.
    fn apply_installed(
        &self,
        i: usize,
        run: u64,
        updates: &[RecordUpdate],
        usn: i64,
    ) -> Option<(Duration, usize, usize)> {
        let slot = &self.volumes[i];
        let prepared = slot.index.read().unwrap().prepare_updates(updates);
        let t = Instant::now();
        let (entries, bytes) = {
            let mut index = slot.index.write().unwrap();
            if !self.is_current(i, run) {
                return None;
            }
            index.use_prepared(prepared);
            index.apply_updates(updates, usn);
            (index.file_count(), index.heap_bytes())
        };
        let apply = t.elapsed();

        let compacted = {
            let index = slot.index.read().unwrap();
            index.needs_compaction().then(|| index.compacted())
        };
        if let Some(compacted) = compacted {
            let mut index = slot.index.write().unwrap();
            if !self.is_current(i, run) {
                return None;
            }
            index.use_compacted(compacted);
        }
        Some((apply, entries, bytes))
    }

    /// Keeps volume `i` indexed until it is disabled or the service stops.
    fn run_volume(&self, i: usize, run: u64) {
        if self.offline {
            self.run_offline(i, run);
            return;
        }

        let mut rt = Runtime::default();
        let mut retry = RETRY_FIRST;
        self.prepare(i, run, &mut rt);
        loop {
            if !self.is_current(i, run) {
                return;
            }
            if self.is_stopping() {
                self.finish(i, run, &mut rt);
                return;
            }

            if rt.failed {
                rt.set_background(true);
                self.wait_retry(i, run, retry);
                if !self.is_current(i, run) || self.is_stopping() {
                    continue;
                }
                retry = (retry * 2).min(RETRY_MAX);
                log::info!("Trying {} again", self.volumes[i].volume.id);
                rt = Runtime::default();
                self.prepare(i, run, &mut rt);
                continue;
            }

            if self.is_active() {
                rt.set_background(false);
                if let Err(e) = self.wake_up(i, run, &mut rt) {
                    self.fail(i, run, &mut rt, e);
                    continue;
                }
                retry = RETRY_FIRST;
                match self.follow_journal(i, run, &mut rt) {
                    Ok(()) => {}
                    Err(JournalError::Reset) => {
                        log::warn!(
                            "Journal of {} was reset, rebuilding the index",
                            self.volumes[i].volume.id
                        );
                        rt.needs_scan = true;
                    }
                    // Waiting for the journal is cancelled when the app becomes inactive
                    Err(JournalError::Other(e)) if self.should_follow(i, run) => {
                        self.fail(i, run, &mut rt, format!("Live updates stopped: {:#}", e));
                    }
                    Err(JournalError::Other(_)) => {}
                }
                rt.last_active = Instant::now();
            } else {
                // Everything done while inactive runs with low CPU, disk and memory priority
                rt.set_background(true);
                self.wait_inactive(i, run);
                if self.is_active() || self.is_stopping() || !self.is_current(i, run) {
                    continue;
                }
                if rt.loaded && rt.last_active.elapsed() >= *self.unload_after.lock().unwrap() {
                    self.unload(i, run, &mut rt);
                }
                self.check_journal(i, &mut rt);
            }
        }
    }

    /// Marks volume `i` as failed. It is tried again later, see [`RETRY_FIRST`].
    fn fail(&self, i: usize, run: u64, rt: &mut Runtime, error: String) {
        log::error!("Indexing {} failed: {}", self.volumes[i].volume.id, error);
        // Drops the volume handles, the disk may be gone
        *rt = Runtime {
            failed: true,
            ..Runtime::default()
        };
        self.update_stats(i, run, |s| s.state = VolumeState::Failed(error));
    }

    /// Waits before trying a failed volume again, less when a client becomes active.
    fn wait_retry(&self, i: usize, run: u64, delay: Duration) {
        let activations = self.activations.load(Ordering::SeqCst);
        let lock = self.wake_lock.lock().unwrap();
        let _ = self
            .wake
            .wait_timeout_while(lock, delay, |_| {
                self.activations.load(Ordering::SeqCst) == activations
                    && !self.is_stopping()
                    && self.is_current(i, run)
            })
            .unwrap();
    }

    /// Loads the saved index without volume access, it is not updated.
    fn run_offline(&self, i: usize, run: u64) {
        let slot = &self.volumes[i];
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
    }

    /// Waits until a client becomes active, the service stops, the volume is disabled, or
    /// [`IDLE_CHECK`] passed.
    fn wait_inactive(&self, i: usize, run: u64) {
        let check = *self.idle_check.lock().unwrap();
        let lock = self.wake_lock.lock().unwrap();
        let _ = self
            .wake
            .wait_timeout_while(lock, check, |_| {
                !self.is_active() && !self.is_stopping() && self.is_current(i, run)
            })
            .unwrap();
    }

    fn should_follow(&self, i: usize, run: u64) -> bool {
        self.is_active() && !self.is_stopping() && self.is_current(i, run)
    }

    /// Starts reading the journal where the saved index left off, without loading it. The
    /// index is loaded when a client becomes active.
    fn prepare(&self, i: usize, run: u64, rt: &mut Runtime) {
        let volume = self.volumes[i].volume;
        // Not there (an unplugged disk, a locked BitLocker drive), tried again later
        let handle = match volume.open(false, false) {
            Ok(handle) => handle,
            Err(e) => {
                self.fail(i, run, rt, format!("{:#}", e));
                return;
            }
        };
        let saved = || -> Result<Option<(u64, JournalFollower)>> {
            let Some(header) = read_header(volume, &self.db_dir)? else {
                return Ok(None);
            };
            ensure!(
                header.volume_serial == volume_data(&handle)?.VolumeSerialNumber as u64,
                "Index file is for another volume"
            );
            let journal = query_journal(&handle)?;
            ensure!(header.journal_id == journal.id, "The journal was recreated");
            ensure!(
                header.next_usn >= journal.first_usn && header.next_usn <= journal.next_usn,
                "The journal no longer contains all changes since the index was saved"
            );
            let follower = JournalFollower::open(
                volume,
                header.journal_id,
                header.next_usn,
                header.record_size,
            )?;
            Ok(Some((header.volume_serial, follower)))
        };
        let saved_index = match saved() {
            Ok(Some((serial, follower))) => {
                rt.serial = serial;
                rt.follower = Some(follower);
                SavedIndex::NotChecked
            }
            Ok(None) => {
                rt.needs_scan = true;
                SavedIndex::Missing
            }
            Err(e) => {
                log::info!("Not using saved index of {}: {:#}", volume.id, e);
                rt.needs_scan = true;
                SavedIndex::Discarded(format!("{:#}", e))
            }
        };
        self.update_stats(i, run, |s| {
            s.state = VolumeState::Asleep;
            s.saved_index = saved_index;
        });
    }

    /// Makes the index of volume `i` searchable and up to date: catches up with the journal,
    /// loads the saved index if it was dropped, or scans the volume if nothing else works.
    fn wake_up(&self, i: usize, run: u64, rt: &mut Runtime) -> Result<(), String> {
        let slot = &self.volumes[i];
        if !rt.needs_scan {
            match rt.read_journal() {
                Ok(()) => {}
                Err(JournalError::Reset) => rt.needs_scan = true,
                Err(JournalError::Other(e)) => return Err(format!("{:#}", e)),
            }
        }
        if !rt.loaded && rt.since_save.len() > MAX_PENDING {
            rt.needs_scan = true;
        }
        if rt.needs_scan {
            return self.full_scan(i, run, rt);
        }
        let Some(follower) = rt.follower.as_ref() else {
            return self.full_scan(i, run, rt);
        };
        let usn = follower.reader.next_usn();

        if !rt.loaded {
            self.update_stats(i, run, |s| s.state = VolumeState::Loading);
            let t = Instant::now();
            let mut index = match load_saved(slot.volume, &self.db_dir, rt.serial) {
                Ok(index) => index,
                Err(e) => {
                    log::info!("Not using saved index of {}: {:#}", slot.volume.id, e);
                    self.update_stats(i, run, |s| {
                        s.saved_index = SavedIndex::Discarded(format!("{:#}", e))
                    });
                    rt.needs_scan = true;
                    return self.full_scan(i, run, rt);
                }
            };
            let load = t.elapsed();
            let records = rt.since_save.to_vec();
            let batch = apply(&mut index, follower, &records, usn);
            if !records.is_empty() {
                slot.dirty.store(true, Ordering::Release);
            }
            if !self.install(i, run, index, VolumeState::Ready) {
                return Ok(());
            }
            log::info!(
                "Loaded {} in {:?} and applied {} changed records in {:?}",
                slot.volume.id,
                load,
                records.len(),
                batch.fetch + batch.apply
            );
            self.update_stats(i, run, |s| {
                s.load = Some(load);
                s.saved_index = SavedIndex::Loaded;
                s.catch_up = Some(batch);
            });
            rt.loaded = true;
            self.trim();
        } else {
            let records = rt.since_index.to_vec();
            // Read before taking the write lock, so searches go on meanwhile
            let t = Instant::now();
            let updates = follower.fetch(&records);
            let fetch = t.elapsed();
            let Some((apply, entries, bytes)) = self.apply_installed(i, run, &updates, usn) else {
                return Ok(());
            };
            let batch = BatchStats {
                records: records.len(),
                updates: updates.len(),
                fetch,
                apply,
                at: SystemTime::now(),
            };
            if !records.is_empty() {
                slot.dirty.store(true, Ordering::Release);
            }
            self.update_stats(i, run, |s| {
                s.state = VolumeState::Ready;
                s.entries = entries;
                s.index_bytes = bytes;
                if !records.is_empty() {
                    s.catch_up = Some(batch);
                }
            });
        }
        rt.since_index.clear();
        Ok(())
    }

    /// Builds the index from the MFT and saves it, which is the base for loading it again
    /// after it was dropped from memory.
    fn full_scan(&self, i: usize, run: u64, rt: &mut Runtime) -> Result<(), String> {
        let slot = &self.volumes[i];
        self.update_stats(i, run, |s| s.state = VolumeState::Indexing);
        let (index, scan) =
            scan_volume(slot.volume, &ScanOptions::default()).map_err(|e| format!("{:#}", e))?;
        log::info!("Indexed {} in {:?}", slot.volume.id, scan.total());
        let follower = JournalFollower::new(&index).map_err(|e| format!("{:#}", e))?;
        rt.serial = index.volume_serial;
        if !self.install(i, run, index, VolumeState::Ready) {
            return Ok(());
        }
        self.update_stats(i, run, |s| s.scan = Some(scan));
        rt.follower = Some(follower);
        rt.needs_scan = false;
        rt.loaded = true;
        rt.since_save.clear();
        rt.since_index.clear();
        slot.dirty.store(true, Ordering::Release);
        self.save(i);
        // The scan's buffers
        self.trim();
        Ok(())
    }

    /// Applies journal changes to volume `i` while a client is active.
    fn follow_journal(&self, i: usize, run: u64, rt: &mut Runtime) -> Result<(), JournalError> {
        let slot = &self.volumes[i];
        let Runtime {
            follower,
            since_save,
            ..
        } = rt;
        let Some(follower) = follower.as_mut() else {
            return Ok(());
        };

        loop {
            // Set before checking, so a client becoming inactive either sees it and cancels
            // the wait, or this sees the client inactive
            slot.blocking.store(true, Ordering::SeqCst);
            if !self.should_follow(i, run) {
                slot.blocking.store(false, Ordering::SeqCst);
                return Ok(());
            }
            let changed = follower.wait_for_changes();
            slot.blocking.store(false, Ordering::SeqCst);
            let changed = changed?;
            for &record in &changed {
                since_save.insert(record);
            }

            let t = Instant::now();
            let updates = follower.fetch(&changed);
            let fetch = t.elapsed();
            let usn = follower.reader.next_usn();
            let Some((apply, entries, bytes)) = self.apply_installed(i, run, &updates, usn) else {
                return Ok(());
            };
            slot.dirty.store(true, Ordering::Release);

            let batch = BatchStats {
                records: changed.len(),
                updates: updates.len(),
                fetch,
                apply,
                at: SystemTime::now(),
            };
            self.update_stats(i, run, |s| {
                s.entries = entries;
                s.index_bytes = bytes;
                s.last_batch = Some(batch);
                s.batches += 1;
                s.records_updated += updates.len() as u64;
            });
        }
    }

    /// While inactive: reads the journal once enough of it is new that it could wrap before
    /// the next check, remembering which records changed.
    fn check_journal(&self, i: usize, rt: &mut Runtime) {
        if rt.needs_scan {
            return;
        }
        let Some(follower) = rt.follower.as_ref() else {
            return;
        };
        let reader = &follower.reader;
        let journal = match reader.query() {
            Ok(journal) => journal,
            Err(e) => {
                log::warn!(
                    "Checking the journal of {} failed: {:#}",
                    self.volumes[i].volume.id,
                    e
                );
                return;
            }
        };
        if journal.id != reader.journal_id() || reader.next_usn() < journal.first_usn {
            log::warn!(
                "Journal of {} was reset, the index is rebuilt when the app is used",
                self.volumes[i].volume.id
            );
            rt.needs_scan = true;
            return;
        }
        let new = journal.next_usn.saturating_sub(reader.next_usn()) as u64;
        if new > journal.max_size / READ_AT_FRACTION {
            if let Err(JournalError::Reset) = rt.read_journal() {
                rt.needs_scan = true;
            }
        }
    }

    /// Drops the index from memory. The saved index plus the records changed since it was
    /// saved bring it back.
    fn unload(&self, i: usize, run: u64, rt: &mut Runtime) {
        let slot = &self.volumes[i];
        {
            let mut index = slot.index.write().unwrap();
            if !self.is_current(i, run) {
                return;
            }
            *index = VolumeIndex::empty(slot.volume);
        }
        rt.loaded = false;
        rt.since_index.clear();
        log::info!("Unloaded {}, it was not used for a while", slot.volume.id);
        self.update_stats(i, run, |s| {
            s.state = VolumeState::Asleep;
            s.index_bytes = 0;
        });
        self.trim();
    }

    /// When the service stops: brings the index up to date and saves it if anything changed.
    fn finish(&self, i: usize, run: u64, rt: &mut Runtime) {
        if rt.needs_scan || rt.follower.is_none() {
            return;
        }
        rt.set_background(false);
        if rt.read_journal().is_err() {
            return;
        }
        // Nothing changed since the index was saved
        if !rt.loaded && rt.since_save.is_empty() {
            return;
        }
        if let Err(e) = self.wake_up(i, run, rt) {
            log::error!(
                "Updating {} before saving failed: {}",
                self.volumes[i].volume.id,
                e
            );
            return;
        }
        self.save(i);
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
            Ok(bytes) => {
                log::info!("Saved {} in {:?}", slot.volume.id, t.elapsed());
                self.update_stats(i, run, |s| {
                    s.last_save = Some(SaveStats {
                        took: t.elapsed(),
                        bytes,
                        at: SystemTime::now(),
                    })
                })
            }
            Err(e) => {
                slot.dirty.store(true, Ordering::Release);
                log::error!("Failed to save index of {}: {:#}", slot.volume.id, e);
            }
        }
    }
}

/// Fetches the current state of `records` and applies it to `index`.
fn apply(
    index: &mut VolumeIndex,
    follower: &JournalFollower,
    records: &[u32],
    usn: i64,
) -> BatchStats {
    let t = Instant::now();
    let updates = follower.fetch(records);
    let fetch = t.elapsed();
    let t = Instant::now();
    index.apply_updates(&updates, usn);
    if index.needs_compaction() {
        index.compact_names();
    }
    BatchStats {
        records: records.len(),
        updates: updates.len(),
        fetch,
        apply: t.elapsed(),
        at: SystemTime::now(),
    }
}

/// State of a volume thread.
struct Runtime {
    /// Reads the journal from where the index (in memory or saved) is up to date
    follower: Option<JournalFollower>,
    /// The index is in memory
    loaded: bool,
    /// Nothing can bring the index up to date anymore, only a full scan
    needs_scan: bool,
    /// Indexing failed, it is tried again later
    failed: bool,
    /// Serial number of the volume the saved index belongs to
    serial: u64,
    /// Records changed since the index was saved
    since_save: RecordSet,
    /// Records changed since they were last applied to the index in memory
    since_index: RecordSet,
    /// When a client was last active
    last_active: Instant,
    /// The thread runs in background mode
    background: bool,
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            follower: None,
            loaded: false,
            needs_scan: false,
            failed: false,
            serial: 0,
            since_save: RecordSet::default(),
            since_index: RecordSet::default(),
            last_active: Instant::now(),
            background: false,
        }
    }
}

impl Runtime {
    /// Reads the journal without waiting and remembers the changed records.
    fn read_journal(&mut self) -> Result<(), JournalError> {
        let Some(follower) = self.follower.as_mut() else {
            return Ok(());
        };
        let changed = follower.poll_changes()?;
        for &record in &changed {
            self.since_save.insert(record);
            self.since_index.insert(record);
        }
        Ok(())
    }

    /// Background mode lowers the thread's CPU, disk and memory priority.
    fn set_background(&mut self, on: bool) {
        if self.background != on {
            self.background = on;
            unsafe {
                let mode = if on {
                    THREAD_MODE_BACKGROUND_BEGIN
                } else {
                    THREAD_MODE_BACKGROUND_END
                };
                let _ = SetThreadPriority(GetCurrentThread(), mode);
            }
        }
    }
}

/// A set of record numbers as a bitset, a few hundred KB for millions of records.
#[derive(Default)]
struct RecordSet {
    bits: Vec<u64>,
    len: usize,
}

impl RecordSet {
    fn insert(&mut self, record: u32) {
        let word = record as usize / 64;
        if word >= self.bits.len() {
            self.bits.resize(word + 1, 0);
        }
        let mask = 1u64 << (record % 64);
        if self.bits[word] & mask == 0 {
            self.bits[word] |= mask;
            self.len += 1;
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn to_vec(&self) -> Vec<u32> {
        let mut records = Vec::with_capacity(self.len);
        for (w, &word) in self.bits.iter().enumerate() {
            let mut word = word;
            while word != 0 {
                records.push((w * 64) as u32 + word.trailing_zeros());
                word &= word - 1;
            }
        }
        records
    }

    fn clear(&mut self) {
        self.bits = Vec::new();
        self.len = 0;
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
        Self::open(
            index.volume,
            index.journal_id,
            index.next_usn,
            index.record_size,
        )
    }

    /// Follows the journal from `next_usn` without an index.
    pub fn open(volume: Volume, journal_id: u64, next_usn: i64, record_size: u32) -> Result<Self> {
        Ok(Self {
            reader: JournalReader::open(volume, journal_id, next_usn)?,
            handle: volume.open(true, false)?,
            record_size: record_size as usize,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_set() {
        let mut set = RecordSet::default();
        for r in [5, 700, 5, 64, 0] {
            set.insert(r);
        }
        assert_eq!(set.len(), 4);
        assert_eq!(set.to_vec(), vec![0, 5, 64, 700]);
        set.clear();
        assert!(set.is_empty());
    }
}
