use std::cmp::Ordering;

use rayon::prelude::*;

use crate::index::{is_link, link_id, link_index, SyncPtr, VolumeIndex, FLAG_IN_USE, NO_RECORD};
use crate::ntfs::ROOT_RECORD;

/// First 8 ASCII-lowercased bytes, big endian, so integer order matches [`cmp_names`] for the
/// prefix. NTFS names never contain NUL, so zero padding sorts shorter names first.
#[inline]
pub fn sort_key(name: &[u8]) -> u64 {
    let mut key = [0u8; 8];
    for (k, b) in key.iter_mut().zip(name) {
        *k = b.to_ascii_lowercase();
    }
    u64::from_be_bytes(key)
}

/// Case-insensitive (ASCII) name order with a byte-wise tie break.
#[inline]
pub fn cmp_names(a: &[u8], b: &[u8]) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (x.to_ascii_lowercase(), y.to_ascii_lowercase());
        if x != y {
            return x.cmp(&y);
        }
    }
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

impl VolumeIndex {
    #[inline]
    pub fn cmp_entries(&self, a: u32, b: u32) -> Ordering {
        cmp_names(self.name(a), self.name(b)).then(a.cmp(&b))
    }

    /// Every entry that should be searchable: in-use records except the root, and their links.
    pub(crate) fn searchable_entries(&self) -> Vec<u32> {
        let flags = &self.records.flags;
        let mut ids = (0..flags.len() as u32)
            .into_par_iter()
            .filter(|&id| flags[id as usize] & FLAG_IN_USE != 0 && id != ROOT_RECORD)
            .collect::<Vec<_>>();
        ids.par_extend(
            (0..self.links.len() as u32)
                .into_par_iter()
                .map(link_id)
                .filter(|&id| self.is_in_use(id)),
        );
        ids
    }

    /// Rebuilds [`VolumeIndex::sorted`] from scratch and lays the names out in that order.
    pub fn sort_and_compact(&mut self) {
        let mut keyed = self
            .searchable_entries()
            .into_par_iter()
            .map(|id| (sort_key(self.name(id)), id))
            .collect::<Vec<_>>();

        // Sorting by the integer prefix first avoids touching the names for most comparisons
        keyed.par_sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| self.cmp_entries(a.1, b.1)));

        self.sorted = std::sync::Arc::new(keyed.into_par_iter().map(|(_, id)| id).collect());
        self.compact_names();
    }

    /// The entries with a name, in the order [`VolumeIndex::compact_names`] lays them out: the
    /// sorted entries, then the root.
    pub fn name_order(&self) -> Vec<u32> {
        let mut order = Vec::with_capacity(self.sorted.len() + 1);
        order.extend_from_slice(&self.sorted);
        if self.records.len() > ROOT_RECORD as usize {
            order.push(ROOT_RECORD);
        }
        order
    }

    /// Whether enough of the names are garbage to rewrite them, see [`VolumeIndex::compacted`].
    pub fn needs_compaction(&self) -> bool {
        self.garbage > (1 << 20) && self.garbage > self.names.len() / 4
    }

    /// Rewrites the name arena in sorted order, dropping names that are no longer referenced.
    /// Searching in sorted order then reads the arena mostly sequentially.
    pub fn compact_names(&mut self) {
        let compacted = self.compacted();
        self.use_compacted(compacted);
    }

    /// The names rewritten in sorted order without the garbage, and the new name offsets.
    ///
    /// Takes tens of milliseconds for large volumes. The thread that changes the index calls it
    /// with read access, so searches can go on meanwhile, and swaps the result in with
    /// [`VolumeIndex::use_compacted`].
    pub fn compacted(&self) -> Compacted {
        let order = self.name_order();
        let mut offsets = order
            .par_iter()
            .map(|&id| self.name(id).len() as u32)
            .collect::<Vec<_>>();
        let mut total = 0u32;
        for o in offsets.iter_mut() {
            let len = *o;
            *o = total;
            total += len;
        }

        let mut names = vec![0u8; total as usize];
        let mut record_off = self.records.name_off.clone();
        let (mut link_off, mut link_len) =
            (self.links.name_off.clone(), self.links.name_len.clone());
        // Free link slots end up with an empty name
        for (l, &r) in self.links.record.iter().enumerate() {
            if r == NO_RECORD {
                (link_off[l], link_len[l]) = (0, 0);
            }
        }

        let dst = SyncPtr(names.as_mut_ptr());
        let rec_off = SyncPtr(record_off.as_mut_ptr());
        let link_off_ptr = SyncPtr(link_off.as_mut_ptr());
        // Every id appears once, so the writes are disjoint
        order
            .par_iter()
            .zip(&offsets)
            .for_each(|(&id, &new_off)| unsafe {
                let name = self.name(id);
                let off_ptr = if is_link(id) {
                    link_off_ptr.get().add(link_index(id))
                } else {
                    rec_off.get().add(id as usize)
                };
                std::ptr::copy_nonoverlapping(
                    name.as_ptr(),
                    dst.get().add(new_off as usize),
                    name.len(),
                );
                *off_ptr = new_off;
            });

        Compacted {
            names,
            record_off,
            link_off,
            link_len,
            from: self.compaction_state(),
        }
    }

    /// Takes the result of [`VolumeIndex::compacted`], unless the index changed since.
    pub fn use_compacted(&mut self, compacted: Compacted) -> bool {
        if compacted.from != self.compaction_state() {
            return false;
        }
        self.names = compacted.names;
        self.records.name_off = compacted.record_off;
        self.links.name_off = compacted.link_off;
        self.links.name_len = compacted.link_len;
        self.garbage = 0;
        true
    }

    /// Changes whenever names, records or links change
    fn compaction_state(&self) -> [usize; 4] {
        [
            self.names.as_ptr() as usize,
            self.names.len(),
            self.records.len(),
            self.links.len(),
        ]
    }
}

/// Names rewritten without garbage, see [`VolumeIndex::compacted`].
pub struct Compacted {
    names: Vec<u8>,
    record_off: Vec<u32>,
    link_off: Vec<u32>,
    link_len: Vec<u16>,
    from: [usize; 4],
}
