//! Applying journal changes to a [`VolumeIndex`].

use std::cmp::Ordering;

use crate::index::build::flags_and_size;
use crate::index::{
    is_link, link_id, link_index, VolumeIndex, FLAG_HAS_LINKS, FLAG_IN_USE, FLAG_SIZE_GUESSED,
    NO_RECORD,
};
use crate::ntfs::io::Handle;
use crate::ntfs::record::{
    attribute_list_records, filetime_to_unix, fixup_if_needed, parse_record, primary_name,
    push_utf16le, ParsedRecord, ATTR_DATA, ATTR_FILE_NAME,
};
use crate::ntfs::usn::read_file_record;
use crate::ntfs::ROOT_RECORD;

/// The current state of a record, or `None` if it was deleted.
pub struct RecordUpdate {
    pub record: u32,
    pub state: Option<RecordState>,
}

pub struct RecordState {
    /// Parent and UTF-8 name of every name of the record, the primary one first. Empty if the
    /// names could not be read; the indexed names are kept then.
    pub names: Vec<(u32, Vec<u8>)>,
    pub flags: u32,
    /// `None` for directories (their size is aggregated) and when the size is unknown
    pub size: Option<u64>,
    pub created: u32,
    pub modified: u32,
    pub sequence: u16,
}

/// Reads the current state of a record (and its extension records) from NTFS. Returns `None` if
/// it could not be read.
pub fn fetch_update(handle: &Handle, record: u32, record_size: usize) -> Option<RecordUpdate> {
    let deleted = Some(RecordUpdate { record, state: None });
    let mut rec = match read_file_record(handle, record, record_size) {
        Ok(Some(rec)) => rec,
        Ok(None) => return deleted,
        Err(_) => return None,
    };

    let mut parsed = ParsedRecord::default();
    let mut refs = Vec::new();
    if !fixup_if_needed(&mut rec) || !parse_record(&rec, &mut parsed, &mut refs) {
        return deleted;
    }
    if parsed.base_record != 0 {
        // Journal records always name the base record, so this was reused as an extension
        return deleted;
    }

    let decode = |rec: &[u8], refs: &[crate::ntfs::record::NameRef], out: &mut Vec<(u32, Vec<u8>)>| {
        let primary = primary_name(refs);
        let order = primary.into_iter().chain((0..refs.len()).filter(|&j| Some(j) != primary));
        for j in order {
            let n = refs[j];
            if n.parent <= u32::MAX as u64 {
                let mut name = Vec::with_capacity(n.units);
                push_utf16le(n.bytes(rec), &mut name);
                out.push((n.parent as u32, name));
            }
        }
    };

    let mut names = Vec::new();
    decode(&rec, &refs, &mut names);
    let fn_size = refs.first().map_or(0, |n| n.size);
    let mut data_size = parsed.data_size;

    // Names and the data size may live in extension records
    if let Some((start, end)) = parsed.attribute_list {
        let list = rec[start..end].to_vec();
        for ext in attribute_list_records(&list, &[ATTR_FILE_NAME, ATTR_DATA], record as u64) {
            let Ok(Some(mut ext_rec)) = read_file_record(handle, ext as u32, record_size) else {
                continue;
            };
            let mut ext_parsed = ParsedRecord::default();
            if fixup_if_needed(&mut ext_rec) && parse_record(&ext_rec, &mut ext_parsed, &mut refs) {
                decode(&ext_rec, &refs, &mut names);
                data_size = data_size.or(ext_parsed.data_size);
            }
        }
    }
    parsed.data_size = data_size;

    let (flags, size) = flags_and_size(&parsed, fn_size);
    // A non-resident attribute list could hide the $DATA attribute, keep the old size then
    let size_known = !parsed.is_directory
        && (flags & FLAG_SIZE_GUESSED == 0 || !parsed.has_attribute_list);

    Some(RecordUpdate {
        record,
        state: Some(RecordState {
            names,
            flags,
            size: size_known.then_some(size),
            created: filetime_to_unix(parsed.created),
            modified: filetime_to_unix(parsed.modified),
            sequence: parsed.sequence,
        }),
    })
}

impl VolumeIndex {
    /// Applies record updates and moves the index to `next_usn`.
    pub fn apply_updates(&mut self, updates: &[RecordUpdate], next_usn: i64) {
        let mut touched = Vec::with_capacity(updates.len());

        for i in parents_first(updates) {
            let update = &updates[i];
            let id = update.record;
            let r = id as usize;
            let in_index = self.is_in_use(id);
            // Unless the sequence number matches, the record was deleted and reused for a
            // different file since we last saw it
            let was_in_use = match &update.state {
                Some(s) => in_index && self.records.sequence[r] == s.sequence,
                None => false,
            };
            // A new file is only usable if we know its name
            let state = update
                .state
                .as_ref()
                .filter(|s| was_in_use || !s.names.is_empty());
            if !in_index && state.is_none() {
                continue;
            }
            if r >= self.records.len() {
                self.records.resize(r + 1);
            }

            // Take the record out of the folder sizes, and put it back in after the update
            if in_index {
                self.add_to_ancestors(id, -(self.records.size[r] as i64));
                if !was_in_use {
                    self.remove_links(id, &mut touched);
                    self.records.flags[r] &= !FLAG_IN_USE;
                    self.garbage += self.records.name_len[r] as usize;
                    touched.push(id);
                }
            }
            let Some(state) = state else { continue };

            let mut flags = state.flags;
            if let Some(((parent, name), links)) = state.names.split_first() {
                if !was_in_use || self.name(id) != &name[..] {
                    if was_in_use {
                        self.garbage += self.records.name_len[r] as usize;
                    }
                    (self.records.name_off[r], self.records.name_len[r]) = self.push_name(name);
                }
                self.records.parent[r] = *parent;

                self.remove_links(id, &mut touched);
                for (parent, name) in links {
                    let (off, len) = self.push_name(name);
                    touched.push(link_id(self.links.push(id, *parent, off, len)));
                }
                if !links.is_empty() {
                    flags |= FLAG_HAS_LINKS;
                }
            } else {
                flags |= self.records.flags[r] & FLAG_HAS_LINKS;
            }

            self.records.flags[r] = flags;
            match state.size {
                Some(size) => self.records.size[r] = size,
                None if !was_in_use => self.records.size[r] = 0,
                // Directory sizes are aggregated, unknown sizes are kept
                None => {}
            }
            self.records.created[r] = state.created;
            self.records.modified[r] = state.modified;
            self.records.sequence[r] = state.sequence;
            touched.push(id);

            self.add_to_ancestors(id, self.records.size[r] as i64);
        }

        self.next_usn = next_usn;
        if !touched.is_empty() {
            self.resort(touched);
        }
        if self.garbage > (1 << 20) && self.garbage > self.names.len() / 4 {
            self.compact_names();
        }
    }

    fn push_name(&mut self, name: &[u8]) -> (u32, u16) {
        let off = self.names.len() as u32;
        let len = name.len().min(u16::MAX as usize);
        // Grow in small steps; doubling would add ~100 MB for a few new names. Compaction
        // removes the garbage that accumulates here.
        if self.names.len() + len > self.names.capacity() {
            self.names.reserve_exact(len.max(self.names.capacity() / 64).max(4096));
        }
        self.names.extend_from_slice(&name[..len]);
        (off, len as u16)
    }

    fn remove_links(&mut self, record: u32, touched: &mut Vec<u32>) {
        let r = record as usize;
        if self.records.flags[r] & FLAG_HAS_LINKS == 0 {
            return;
        }
        for l in 0..self.links.len() {
            if self.links.record[l] == record {
                self.links.record[l] = NO_RECORD;
                self.garbage += self.links.name_len[l] as usize;
                self.links.free.push(l as u32);
                touched.push(link_id(l as u32));
            }
        }
        self.records.flags[r] &= !FLAG_HAS_LINKS;
    }

    /// Moves the touched entries to their new position in [`VolumeIndex::sorted`].
    fn resort(&mut self, mut touched: Vec<u32>) {
        touched.sort_unstable();
        touched.dedup();

        let mut records = vec![0u64; self.records.len().div_ceil(64)];
        let mut links = vec![0u64; self.links.len().div_ceil(64)];
        for &id in &touched {
            let (set, i) = if is_link(id) {
                (&mut links, link_index(id))
            } else {
                (&mut records, id as usize)
            };
            set[i / 64] |= 1 << (i % 64);
        }
        self.sorted.retain(|&id| {
            let (set, i) = if is_link(id) {
                (&links, link_index(id))
            } else {
                (&records, id as usize)
            };
            set[i / 64] & (1 << (i % 64)) == 0
        });

        let mut new = touched
            .into_iter()
            .filter(|&id| id != ROOT_RECORD && self.is_in_use(id))
            .collect::<Vec<_>>();
        if new.is_empty() {
            return;
        }
        new.sort_unstable_by(|&a, &b| self.cmp_entries(a, b));

        // Binary search the insertion points and copy the runs in between
        let mut out = Vec::with_capacity(self.sorted.len() + new.len());
        let mut start = 0;
        for id in new {
            let pos = start
                + self.sorted[start..]
                    .partition_point(|&x| self.cmp_entries(x, id) == Ordering::Less);
            out.extend_from_slice(&self.sorted[start..pos]);
            out.push(id);
            start = pos;
        }
        out.extend_from_slice(&self.sorted[start..]);
        self.sorted = out;
    }
}

/// Orders updates so that a directory is applied before the entries inside it. A new directory
/// starts with size 0, so entries applied before it would be missing from its folder size.
fn parents_first(updates: &[RecordUpdate]) -> Vec<usize> {
    let pos = updates
        .iter()
        .enumerate()
        .map(|(i, u)| (u.record, i))
        .collect::<std::collections::HashMap<_, _>>();
    let parents = |i: usize| {
        updates[i]
            .state
            .iter()
            .flat_map(|s| s.names.iter().map(|(p, _)| *p))
            .filter_map(|p| pos.get(&p).copied())
            .filter(move |&j| j != i)
            .collect::<Vec<_>>()
    };

    // Iterative depth first search, emitting parents before children
    const NEW: u8 = 0;
    const VISITING: u8 = 1;
    const DONE: u8 = 2;
    let mut state = vec![NEW; updates.len()];
    let mut order = Vec::with_capacity(updates.len());
    for start in 0..updates.len() {
        if state[start] != NEW {
            continue;
        }
        let mut stack = vec![(start, parents(start))];
        state[start] = VISITING;
        while let Some((i, pending)) = stack.last_mut() {
            match pending.pop() {
                Some(j) if state[j] == NEW => {
                    state[j] = VISITING;
                    let p = parents(j);
                    stack.push((j, p));
                }
                Some(_) => {}
                None => {
                    state[*i] = DONE;
                    order.push(*i);
                    stack.pop();
                }
            }
        }
    }
    order
}
