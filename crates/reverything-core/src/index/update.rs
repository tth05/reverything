//! Applying journal changes to a [`VolumeIndex`].

use std::cmp::Ordering;
use std::collections::HashSet;
use std::sync::Arc;

use fixedbitset::FixedBitSet;
use rayon::prelude::*;
use tracing::info_span;

use crate::index::build::flags_and_size;
use crate::index::{
    is_link, link_id, link_index, Records, VolumeIndex, FLAG_DIRECTORY, FLAG_HAS_LINKS,
    FLAG_IN_USE, FLAG_SIZE_GUESSED, NO_RECORD,
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
    let deleted = Some(RecordUpdate {
        record,
        state: None,
    });
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

    let decode =
        |rec: &[u8], refs: &[crate::ntfs::record::NameRef], out: &mut Vec<(u32, Vec<u8>)>| {
            let primary = primary_name(refs);
            let order = primary
                .into_iter()
                .chain((0..refs.len()).filter(|&j| Some(j) != primary));
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
    let size_known =
        !parsed.is_directory && (flags & FLAG_SIZE_GUESSED == 0 || !parsed.has_attribute_list);

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
    /// Applies record updates and moves the index to `next_usn`. The phases run in `tracing`
    /// spans named `update.*`.
    pub fn apply_updates(&mut self, updates: &[RecordUpdate], next_usn: i64) {
        let _span = info_span!("update").entered();
        let records = info_span!("update.records").entered();
        let mut touched = Vec::with_capacity(updates.len());
        let order = parents_first(updates);

        // Directories before the update, to keep the locations for ranking up to date
        let directories = order
            .iter()
            .map(|&i| &updates[i])
            .filter(|u| {
                u.state
                    .as_ref()
                    .is_some_and(|s| s.flags & FLAG_DIRECTORY != 0)
                    || self.is_directory(u.record)
            })
            .map(|u| {
                let before = self
                    .is_directory(u.record)
                    .then(|| (self.parent(u.record), self.name(u.record).to_vec()));
                (u.record, before)
            })
            .collect::<Vec<_>>();

        // Where the affected entries are, while the order is still intact
        let before = info_span!("update.locate").in_scope(|| self.locate(updates));

        for i in order {
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
                self.records.grow_to(r + 1);
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
            // The order is by name, so only new entries and new names move
            let mut moved = !was_in_use;
            if let Some(((parent, name), links)) = state.names.split_first() {
                if !was_in_use || self.name(id) != &name[..] {
                    if was_in_use {
                        self.garbage += self.records.name_len[r] as usize;
                    }
                    (self.records.name_off[r], self.records.name_len[r]) = self.push_name(name);
                    moved = true;
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
            if moved {
                touched.push(id);
            }

            self.add_to_ancestors(id, self.records.size[r] as i64);
        }

        drop(records);

        info_span!("update.locations").in_scope(|| self.update_locations(&directories));
        if !directories.is_empty() {
            *self.folder_ranks.get_mut().unwrap() = None;
        }
        self.next_usn = next_usn;
        if !touched.is_empty() {
            info_span!("update.resort").in_scope(|| self.resort(touched, before));
        }
    }

    fn push_name(&mut self, name: &[u8]) -> (u32, u16) {
        let off = self.names.len() as u32;
        let len = name.len().min(u16::MAX as usize);
        if self.names.len() + len > self.names.capacity() {
            let _span = info_span!("update.grow_names").entered();
            self.names.reserve_exact(name_growth(&self.names, len));
        }
        self.names.extend_from_slice(&name[..len]);
        (off, len as u16)
    }

    /// Grows the names and records ahead of `updates`, if they need more room than there is.
    ///
    /// Growing copies them all, which takes tens of milliseconds for large volumes. The thread
    /// that applies the updates calls this with read access, so searches can go on meanwhile,
    /// then hands the result to [`VolumeIndex::use_prepared`] with write access.
    pub fn prepare_updates(&self, updates: &[RecordUpdate]) -> Prepared {
        let _span = info_span!("update.prepare").entered();
        let adds = updates
            .iter()
            .filter_map(|u| u.state.as_ref())
            .flat_map(|s| &s.names)
            .map(|(_, name)| name.len().min(u16::MAX as usize))
            .sum::<usize>();
        let names = (self.names.len() + adds > self.names.capacity()).then(|| {
            let mut names = Vec::with_capacity(self.names.len() + name_growth(&self.names, adds));
            names.extend_from_slice(&self.names);
            (names, self.names_state())
        });
        let records = updates
            .iter()
            .map(|u| u.record as usize + 1)
            .max()
            .filter(|&n| n > self.records.flags.capacity())
            .map(|n| (self.records.with_room(n), self.records_state()));
        Prepared { names, records }
    }

    /// Takes what [`VolumeIndex::prepare_updates`] grew, unless the index changed since.
    pub fn use_prepared(&mut self, prepared: Prepared) {
        if let Some((names, from)) = prepared.names {
            if from == self.names_state() {
                self.names = names;
            }
        }
        if let Some((records, from)) = prepared.records {
            if from == self.records_state() {
                self.records = records;
            }
        }
    }

    /// Identifies the names, to tell whether they changed: any change moves or resizes them.
    fn names_state(&self) -> (usize, usize) {
        (self.names.as_ptr() as usize, self.names.len())
    }

    fn records_state(&self) -> (usize, usize) {
        (self.records.flags.as_ptr() as usize, self.records.len())
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

    /// The positions in [`VolumeIndex::sorted`] of the entries `updates` may move: the records
    /// and their hard links. `None` for batches large enough that going through the whole list
    /// is cheaper, or if an entry is not where the order says it is.
    fn locate(&self, updates: &[RecordUpdate]) -> Option<Vec<(u32, usize)>> {
        if updates.len() > LOCATE_MAX.min(self.sorted.len() / 8) {
            return None;
        }
        let mut ids = updates
            .iter()
            .map(|u| u.record)
            .filter(|&id| id != ROOT_RECORD && self.is_in_use(id))
            .collect::<Vec<_>>();
        let with_links = ids
            .iter()
            .copied()
            .filter(|&id| self.records.flags[id as usize] & FLAG_HAS_LINKS != 0)
            .collect::<HashSet<_>>();
        if !with_links.is_empty() {
            ids.extend(
                (0..self.links.len() as u32)
                    .filter(|&l| with_links.contains(&self.links.record[l as usize]))
                    .map(link_id),
            );
        }
        ids.sort_unstable();
        ids.dedup();
        ids.into_par_iter()
            .map(|id| {
                let pos = self
                    .sorted
                    .partition_point(|&x| self.cmp_entries(x, id) == Ordering::Less);
                (self.sorted.get(pos) == Some(&id)).then_some((id, pos))
            })
            .collect()
    }

    /// Moves the touched entries to their new position in [`VolumeIndex::sorted`]. `before`
    /// holds the positions [`VolumeIndex::locate`] found before the update.
    fn resort(&mut self, mut touched: Vec<u32>, before: Option<Vec<(u32, usize)>>) {
        touched.sort_unstable();
        touched.dedup();
        let mut new = touched
            .iter()
            .copied()
            .filter(|&id| id != ROOT_RECORD && self.is_in_use(id))
            .collect::<Vec<_>>();
        new.sort_unstable_by(|&a, &b| self.cmp_entries(a, b));

        let Some(before) = before else {
            self.resort_all(&touched, &new);
            return;
        };
        // Every touched entry that was in the list is one of the located ones
        let mut removed = touched
            .iter()
            .filter_map(|id| {
                let i = before.binary_search_by_key(id, |&(id, _)| id).ok()?;
                Some(before[i].1)
            })
            .collect::<Vec<_>>();
        removed.sort_unstable();

        let old = &self.sorted;
        // Whether the entry at `p` comes before `id`. Removed entries may have a new name, so
        // they count as the closest kept entry before them.
        let comes_before = |p: usize, id: u32| {
            let (mut q, mut r) = (p, removed.partition_point(|&x| x <= p));
            while r > 0 && removed[r - 1] == q {
                if q == 0 {
                    return true;
                }
                (q, r) = (q - 1, r - 1);
            }
            self.cmp_entries(old[q], id) == Ordering::Less
        };

        // Independent binary searches, in order since `new` is
        let at = new
            .par_iter()
            .map(|&id| {
                let (mut lo, mut hi) = (0, old.len());
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    if comes_before(mid, id) {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
                lo
            })
            .collect::<Vec<_>>();

        // Unless search results hold the list, move the entries in place: a new list of this
        // size can take milliseconds of page faults
        if let Some(sorted) = Arc::get_mut(&mut self.sorted) {
            move_in_place(sorted, &removed, &new, &at);
            return;
        }
        let old = &self.sorted;
        let mut out = Vec::with_capacity(old.len() - removed.len() + new.len());
        let mut start = 0;
        let mut skip = removed.iter().copied().peekable();
        // Copies old[from..to] without the removed entries
        let mut copy = |out: &mut Vec<u32>, mut from: usize, to: usize| {
            while let Some(r) = skip.next_if(|&r| r < to) {
                out.extend_from_slice(&old[from..r]);
                from = r + 1;
            }
            out.extend_from_slice(&old[from..to]);
        };
        for (id, at) in new.into_iter().zip(at) {
            copy(&mut out, start, at);
            out.push(id);
            start = at;
        }
        copy(&mut out, start, old.len());
        self.sorted = Arc::new(out);
    }

    /// [`VolumeIndex::resort`] for large batches: filters the whole list, then inserts `new`.
    fn resort_all(&mut self, touched: &[u32], new: &[u32]) {
        let mut records = FixedBitSet::with_capacity(self.records.len());
        let mut links = FixedBitSet::with_capacity(self.links.len());
        for &id in touched {
            if is_link(id) {
                links.insert(link_index(id));
            } else {
                records.insert(id as usize);
            }
        }
        let untouched = |id: u32| {
            if is_link(id) {
                !links.contains(link_index(id))
            } else {
                !records.contains(id as usize)
            }
        };
        // Search results may still hold the current list
        match Arc::get_mut(&mut self.sorted) {
            Some(sorted) => sorted.retain(|&id| untouched(id)),
            None => {
                let kept = self.sorted.iter().copied().filter(|&id| untouched(id));
                self.sorted = Arc::new(kept.collect());
            }
        }
        if new.is_empty() {
            return;
        }

        // Binary search the insertion points and copy the runs in between
        let mut out = Vec::with_capacity(self.sorted.len() + new.len());
        let mut start = 0;
        for &id in new {
            let pos = start
                + self.sorted[start..]
                    .partition_point(|&x| self.cmp_entries(x, id) == Ordering::Less);
            out.extend_from_slice(&self.sorted[start..pos]);
            out.push(id);
            start = pos;
        }
        out.extend_from_slice(&self.sorted[start..]);
        self.sorted = Arc::new(out);
    }
}

/// Removes the entries at the positions `removed` and inserts `new` before the old positions
/// `at` (both ascending), moving only the entries in between.
fn move_in_place(sorted: &mut Vec<u32>, removed: &[usize], new: &[u32], at: &[usize]) {
    if let Some(&first) = removed.first() {
        let mut write = first;
        for (i, &r) in removed.iter().enumerate() {
            let end = removed.get(i + 1).copied().unwrap_or(sorted.len());
            sorted.copy_within(r + 1..end, write);
            write += end - r - 1;
        }
        sorted.truncate(write);
    }
    if new.is_empty() {
        return;
    }

    // Open the gaps from the back
    let len = sorted.len();
    if len + new.len() > sorted.capacity() {
        sorted.reserve_exact(new.len().max(len / 64));
    }
    sorted.resize(len + new.len(), 0);
    let mut end = len;
    for j in (0..new.len()).rev() {
        // The position without the removed entries
        let a = at[j] - removed.partition_point(|&r| r < at[j]);
        sorted.copy_within(a..end, a + j + 1);
        sorted[a + j] = new[j];
        end = a;
    }
}

/// Copies of the names and records with more room, from [`VolumeIndex::prepare_updates`], each with
/// the state of what it was copied from.
pub struct Prepared {
    names: Option<(Vec<u8>, (usize, usize))>,
    records: Option<(Records, (usize, usize))>,
}

/// How much to grow the names by to fit `len` more bytes. Small steps: doubling would add ~100
/// MB for a few new names. Compaction removes the garbage that accumulates.
fn name_growth(names: &[u8], len: usize) -> usize {
    len.max(names.len() / 64).max(4096)
}

/// Batches of up to this many updates move their entries without going through the whole sorted
/// list, see [`VolumeIndex::locate`]
const LOCATE_MAX: usize = 1 << 16;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::testing::{names, volume};

    fn next(x: &mut u64) -> u64 {
        *x ^= *x << 13;
        *x ^= *x >> 7;
        *x ^= *x << 17;
        *x
    }

    /// A random file record that is in use
    fn random_file(index: &VolumeIndex, x: &mut u64) -> u32 {
        loop {
            let id = (next(x) % index.records.len() as u64) as u32;
            if id != ROOT_RECORD && index.is_in_use(id) {
                return id;
            }
        }
    }

    fn state(names: Vec<Vec<u8>>, sequence: u16, size: u64) -> RecordState {
        RecordState {
            names: names.into_iter().map(|n| (ROOT_RECORD, n)).collect(),
            flags: FLAG_IN_USE,
            size: Some(size),
            created: 1,
            modified: 2,
            sequence,
        }
    }

    /// Random batches of every kind of change, small ones that move entries by their located
    /// positions and large ones that go through the whole list, with and without a search
    /// result holding the list. The order has to match sorting from scratch after each.
    #[test]
    fn updates_keep_the_order() {
        let mut index = volume('A', &names(11, 4000));
        let pool = names(12, 500);
        let mut x = 0x9e3779b97f4a7c15;
        let mut next_record = index.records.len() as u32;
        for round in 0..80 {
            let batch = [1, 3, 20, 90, 700][round % 5];
            let updates = (0..batch)
                .map(|_| {
                    let name =
                        |x: &mut u64| pool[next(x) as usize % pool.len()].clone().into_bytes();
                    let kind = next(&mut x) % 7;
                    let id = random_file(&index, &mut x);
                    let r = id as usize;
                    let (sequence, current) = (index.records.sequence[r], index.name(id).to_vec());
                    match kind {
                        // New size and date only
                        0 => RecordUpdate {
                            record: id,
                            state: Some(state(vec![current], sequence, next(&mut x) % 100)),
                        },
                        1 => RecordUpdate {
                            record: id,
                            state: Some(state(vec![name(&mut x)], sequence, 1)),
                        },
                        2 => RecordUpdate {
                            record: id,
                            state: None,
                        },
                        // Deleted and reused for another file
                        3 => RecordUpdate {
                            record: id,
                            state: Some(state(vec![name(&mut x)], sequence.wrapping_add(1), 1)),
                        },
                        // Hard links, added or kept
                        4 => RecordUpdate {
                            record: id,
                            state: Some(state(
                                vec![current, name(&mut x), name(&mut x)],
                                sequence,
                                1,
                            )),
                        },
                        _ => {
                            next_record += 1;
                            let names = if kind == 5 {
                                vec![name(&mut x)]
                            } else {
                                vec![name(&mut x), name(&mut x)]
                            };
                            RecordUpdate {
                                record: next_record,
                                state: Some(state(names, 1, 1)),
                            }
                        }
                    }
                })
                .collect::<Vec<_>>();
            // Records appear once per batch, like in the journal
            let mut seen = HashSet::new();
            let updates = updates
                .into_iter()
                .filter(|u| seen.insert(u.record))
                .collect::<Vec<_>>();

            let held = (round % 2 == 0).then(|| index.sorted.clone());
            index.apply_updates(&updates, round as i64);
            drop(held);

            let mut expected = index.searchable_entries();
            expected.sort_by(|&a, &b| index.cmp_entries(a, b));
            assert_eq!(
                *index.sorted, expected,
                "round {} with {} updates",
                round, batch
            );

            // Rewriting the names keeps every name
            if round % 10 == 9 {
                let names = |index: &VolumeIndex| {
                    expected
                        .iter()
                        .map(|&id| index.name(id).to_vec())
                        .collect::<Vec<_>>()
                };
                let before = names(&index);
                let compacted = index.compacted();
                assert!(index.use_compacted(compacted));
                assert_eq!(names(&index), before);
                assert_eq!(index.garbage, 0);
                assert_eq!(
                    index.names.len(),
                    before.iter().map(Vec::len).sum::<usize>()
                );
            }
        }
    }
}
