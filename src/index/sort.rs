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
    fn searchable_entries(&self) -> Vec<u32> {
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
        keyed.par_sort_unstable_by(|a, b| {
            a.0.cmp(&b.0).then_with(|| self.cmp_entries(a.1, b.1))
        });

        self.sorted = keyed.into_par_iter().map(|(_, id)| id).collect();
        self.compact_names();
    }

    /// Rewrites the name arena in sorted order, dropping names that are no longer referenced.
    /// Searching in sorted order then reads the arena mostly sequentially.
    pub fn compact_names(&mut self) {
        let mut order = self.sorted.clone();
        if self.records.len() > ROOT_RECORD as usize {
            order.push(ROOT_RECORD);
        }
        // Free link slots end up with an empty name
        for (l, &r) in self.links.record.iter().enumerate() {
            if r == NO_RECORD {
                self.links.name_off[l] = 0;
                self.links.name_len[l] = 0;
            }
        }

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

        let old = std::mem::take(&mut self.names);
        let mut names = vec![0u8; total as usize];
        let dst = SyncPtr(names.as_mut_ptr());
        let (records, links) = (&mut self.records, &mut self.links);
        let rec_off = SyncPtr(records.name_off.as_mut_ptr());
        let link_off = SyncPtr(links.name_off.as_mut_ptr());
        let (rec_len, link_len) = (&records.name_len, &links.name_len);

        // Every id appears once, so the writes are disjoint
        order.par_iter().zip(&offsets).for_each(|(&id, &new_off)| unsafe {
            let (off_ptr, len) = if is_link(id) {
                let l = link_index(id);
                (link_off.get().add(l), link_len[l] as usize)
            } else {
                let i = id as usize;
                (rec_off.get().add(i), rec_len[i] as usize)
            };
            std::ptr::copy_nonoverlapping(
                old.as_ptr().add(*off_ptr as usize),
                dst.get().add(new_off as usize),
                len,
            );
            *off_ptr = new_off;
        });

        self.names = names;
        self.garbage = 0;
    }
}
