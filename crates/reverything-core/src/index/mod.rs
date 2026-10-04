//! In-memory file index of a single NTFS volume.
//!
//! Entries are identified by a `u32` id. Ids below [`LINK_BIT`] are MFT record numbers and use
//! the record's primary name. Ids with [`LINK_BIT`] set point into [`Links`], which holds the
//! additional names of hard linked files.

pub mod build;
pub mod exclude;
pub mod filter;
pub mod persist;
pub mod rank;
pub mod search;
pub mod sizes;
pub mod sort;
pub mod update;

use std::sync::{Arc, Mutex};

use crate::ntfs::volume::Volume;
use crate::ntfs::ROOT_RECORD;

/// Set for records that hold a file or directory.
pub const FLAG_IN_USE: u32 = 1 << 31;
/// No $DATA attribute was found, so the size came from the often stale $FILE_NAME attribute.
pub const FLAG_SIZE_GUESSED: u32 = 1 << 30;
/// The record has additional names in [`Links`].
pub const FLAG_HAS_LINKS: u32 = 1 << 29;
/// FILE_ATTRIBUTE_DIRECTORY
pub const FLAG_DIRECTORY: u32 = 0x10;
/// FILE_ATTRIBUTE_* bits kept from $STANDARD_INFORMATION
pub const ATTRIBUTE_MASK: u32 = 0x00FF_FFFF;

/// Marks entry ids that refer to [`Links`].
pub const LINK_BIT: u32 = 1 << 31;
/// `Links::record` value of unused link slots.
pub const NO_RECORD: u32 = u32::MAX;

/// Raw pointer that may be shared between threads, for parallel writes to disjoint indices.
#[derive(Copy, Clone)]
pub struct SyncPtr<T>(pub *mut T);

unsafe impl<T> Send for SyncPtr<T> {}
unsafe impl<T> Sync for SyncPtr<T> {}

impl<T> SyncPtr<T> {
    /// Accessing through a method makes closures capture the whole wrapper, not the raw field
    #[inline]
    pub fn get(self) -> *mut T {
        self.0
    }
}

/// Guards against corrupt parent chains
pub const MAX_DEPTH: usize = 1024;

/// Per record data stored as separate arrays (struct of arrays), indexed by MFT record number.
#[derive(Default)]
pub struct Records {
    /// Offset of the name in [`VolumeIndex::names`]
    pub name_off: Vec<u32>,
    /// Length of the UTF-8 name in bytes
    pub name_len: Vec<u16>,
    pub parent: Vec<u32>,
    /// `FLAG_*` and FILE_ATTRIBUTE_* bits
    pub flags: Vec<u32>,
    /// File size, or the total size of everything below a directory
    pub size: Vec<u64>,
    /// Unix timestamps in seconds
    pub created: Vec<u32>,
    pub modified: Vec<u32>,
    /// NTFS sequence number, to tell a reused record from an updated one
    pub sequence: Vec<u16>,
}

impl Records {
    pub fn with_len(n: usize) -> Self {
        Self {
            name_off: vec![0; n],
            name_len: vec![0; n],
            parent: vec![0; n],
            flags: vec![0; n],
            size: vec![0; n],
            created: vec![0; n],
            modified: vec![0; n],
            sequence: vec![0; n],
        }
    }

    pub fn len(&self) -> usize {
        self.flags.len()
    }

    pub fn is_empty(&self) -> bool {
        self.flags.is_empty()
    }

    pub fn resize(&mut self, n: usize) {
        self.name_off.resize(n, 0);
        self.name_len.resize(n, 0);
        self.parent.resize(n, 0);
        self.flags.resize(n, 0);
        self.size.resize(n, 0);
        self.created.resize(n, 0);
        self.modified.resize(n, 0);
        self.sequence.resize(n, 0);
    }

    pub fn heap_bytes(&self) -> usize {
        self.name_off.capacity() * 4
            + self.name_len.capacity() * 2
            + self.parent.capacity() * 4
            + self.flags.capacity() * 4
            + self.size.capacity() * 8
            + self.created.capacity() * 4
            + self.modified.capacity() * 4
            + self.sequence.capacity() * 2
    }
}

/// Additional (hard link) names of records.
#[derive(Default)]
pub struct Links {
    /// Record the name belongs to, [`NO_RECORD`] for unused slots
    pub record: Vec<u32>,
    pub parent: Vec<u32>,
    pub name_off: Vec<u32>,
    pub name_len: Vec<u16>,
    /// Unused slots
    pub free: Vec<u32>,
}

impl Links {
    pub fn len(&self) -> usize {
        self.record.len()
    }

    pub fn is_empty(&self) -> bool {
        self.record.is_empty()
    }

    pub fn push(&mut self, record: u32, parent: u32, name_off: u32, name_len: u16) -> u32 {
        if let Some(l) = self.free.pop() {
            let i = l as usize;
            self.record[i] = record;
            self.parent[i] = parent;
            self.name_off[i] = name_off;
            self.name_len[i] = name_len;
            return l;
        }
        self.record.push(record);
        self.parent.push(parent);
        self.name_off.push(name_off);
        self.name_len.push(name_len);
        (self.record.len() - 1) as u32
    }

    pub fn heap_bytes(&self) -> usize {
        self.record.capacity() * 4
            + self.parent.capacity() * 4
            + self.name_off.capacity() * 4
            + self.name_len.capacity() * 2
            + self.free.capacity() * 4
    }
}

#[inline]
pub fn is_link(id: u32) -> bool {
    id & LINK_BIT != 0
}

#[inline]
pub fn link_id(link: u32) -> u32 {
    link | LINK_BIT
}

#[inline]
pub fn link_index(id: u32) -> usize {
    (id & !LINK_BIT) as usize
}

pub struct VolumeIndex {
    pub volume: Volume,
    pub volume_serial: u64,
    pub journal_id: u64,
    /// Journal position the index is up to date with
    pub next_usn: i64,
    pub record_size: u32,
    pub records: Records,
    pub links: Links,
    /// UTF-8 names of all entries back to back
    pub names: Vec<u8>,
    /// In-use entries (except the root) ordered by name
    pub sorted: Vec<u32>,
    /// Bytes in `names` that are no longer referenced
    pub garbage: usize,
    /// Location of every directory for ranking, see [`VolumeIndex::locations`]. Dropped when
    /// directories change.
    pub locations: Mutex<Option<Arc<Vec<u8>>>>,
}

impl VolumeIndex {
    pub fn empty(volume: Volume) -> Self {
        Self {
            volume,
            volume_serial: 0,
            journal_id: 0,
            next_usn: 0,
            record_size: 1024,
            records: Records::default(),
            links: Links::default(),
            names: Vec::new(),
            sorted: Vec::new(),
            garbage: 0,
            locations: Mutex::default(),
        }
    }

    /// The record an entry belongs to.
    #[inline]
    pub fn record_of(&self, id: u32) -> u32 {
        if is_link(id) {
            self.links.record[link_index(id)]
        } else {
            id
        }
    }

    #[inline]
    pub fn name(&self, id: u32) -> &[u8] {
        let (off, len) = if is_link(id) {
            let l = link_index(id);
            (self.links.name_off[l], self.links.name_len[l])
        } else {
            let i = id as usize;
            (self.records.name_off[i], self.records.name_len[i])
        };
        &self.names[off as usize..off as usize + len as usize]
    }

    pub fn name_str(&self, id: u32) -> &str {
        std::str::from_utf8(self.name(id)).unwrap_or("?")
    }

    #[inline]
    pub fn parent(&self, id: u32) -> u32 {
        if is_link(id) {
            self.links.parent[link_index(id)]
        } else {
            self.records.parent[id as usize]
        }
    }

    #[inline]
    pub fn flags(&self, id: u32) -> u32 {
        self.records.flags[self.record_of(id) as usize]
    }

    #[inline]
    pub fn is_in_use(&self, id: u32) -> bool {
        let record = if is_link(id) {
            match self.links.record.get(link_index(id)) {
                Some(&r) if r != NO_RECORD => r,
                _ => return false,
            }
        } else {
            id
        };
        self.records
            .flags
            .get(record as usize)
            .is_some_and(|f| f & FLAG_IN_USE != 0)
    }

    #[inline]
    pub fn size(&self, id: u32) -> u64 {
        self.records.size[self.record_of(id) as usize]
    }

    #[inline]
    pub fn modified(&self, id: u32) -> u32 {
        self.records.modified[self.record_of(id) as usize]
    }

    #[inline]
    pub fn created(&self, id: u32) -> u32 {
        self.records.created[self.record_of(id) as usize]
    }

    /// Number of searchable entries
    pub fn file_count(&self) -> usize {
        self.sorted.len()
    }

    /// Ancestor directories from the direct parent upwards, excluding the root.
    pub fn ancestors(&self, id: u32) -> impl Iterator<Item = u32> + '_ {
        let mut cur = self.parent(id);
        let mut depth = 0;
        std::iter::from_fn(move || {
            if cur == ROOT_RECORD || depth >= MAX_DEPTH || !self.is_in_use(cur) {
                return None;
            }
            let dir = cur;
            cur = self.parent(dir);
            depth += 1;
            Some(dir)
        })
    }

    /// Path of the directory containing `id`, e.g. `C:\Windows`.
    pub fn folder_path(&self, id: u32) -> String {
        let ancestors = self.ancestors(id).collect::<Vec<_>>();
        let mut out = String::with_capacity(
            3 + ancestors
                .iter()
                .map(|&a| self.name(a).len() + 1)
                .sum::<usize>(),
        );
        out.push(self.volume.id.to_ascii_uppercase());
        out.push(':');
        for &dir in ancestors.iter().rev() {
            out.push('\\');
            out.push_str(self.name_str(dir));
        }
        if ancestors.is_empty() {
            out.push('\\');
        }
        out
    }

    pub fn full_path(&self, id: u32) -> String {
        let mut path = self.folder_path(id);
        if !path.ends_with('\\') {
            path.push('\\');
        }
        path.push_str(self.name_str(id));
        path
    }

    /// All entry ids of a record: the record itself followed by its hard links.
    pub fn entries_of(&self, record: u32) -> Vec<u32> {
        let mut ids = vec![record];
        if self.records.flags[record as usize] & FLAG_HAS_LINKS != 0 {
            ids.extend(
                self.links
                    .record
                    .iter()
                    .enumerate()
                    .filter(|(_, &r)| r == record)
                    .map(|(l, _)| link_id(l as u32)),
            );
        }
        ids
    }

    pub fn heap_bytes(&self) -> usize {
        self.records.heap_bytes()
            + self.links.heap_bytes()
            + self.names.capacity()
            + self.sorted.capacity() * 4
    }
}
