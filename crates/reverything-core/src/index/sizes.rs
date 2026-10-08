//! Folder sizes: the size of a directory is the total size of all files below it. Hard linked
//! files count in every directory they appear in, like Explorer does.

use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};

use rayon::prelude::*;

use crate::index::{VolumeIndex, FLAG_DIRECTORY, FLAG_IN_USE, MAX_DEPTH, NO_RECORD};
use crate::ntfs::ROOT_RECORD;

const DIR_IN_USE: u32 = FLAG_IN_USE | FLAG_DIRECTORY;

impl VolumeIndex {
    #[inline]
    fn is_live_dir(&self, record: u32) -> bool {
        self.records
            .flags
            .get(record as usize)
            .is_some_and(|f| f & DIR_IN_USE == DIR_IN_USE)
    }

    /// Computes all folder sizes from scratch in O(n), in parallel.
    pub fn compute_folder_sizes(&mut self) {
        let n = self.records.len();
        let flags = &self.records.flags;
        let parent = &self.records.parent;
        let is_dir = |r: u32| {
            flags
                .get(r as usize)
                .is_some_and(|f| f & DIR_IN_USE == DIR_IN_USE)
        };

        self.records
            .size
            .par_iter_mut()
            .zip(flags.par_iter())
            .filter(|(_, &f)| f & DIR_IN_USE == DIR_IN_USE)
            .for_each(|(s, _)| *s = 0);
        // Directories only grow below; files are only read
        let size = atomics(&mut self.records.size);

        // Direct contributions of files and their hard links
        (0..n).into_par_iter().with_min_len(4096).for_each(|i| {
            if flags[i] & DIR_IN_USE == FLAG_IN_USE && is_dir(parent[i]) {
                let file = size[i].load(Ordering::Relaxed);
                size[parent[i] as usize].fetch_add(file, Ordering::Relaxed);
            }
        });
        for (l, &r) in self.links.record.iter().enumerate() {
            let p = self.links.parent[l];
            if r != NO_RECORD && flags[r as usize] & DIR_IN_USE == FLAG_IN_USE && is_dir(p) {
                let file = size[r as usize].load(Ordering::Relaxed);
                size[p as usize].fetch_add(file, Ordering::Relaxed);
            }
        }

        // Depth of every directory. Threads that walk the same chain write the same depths.
        let depth = (0..n).map(|_| AtomicU16::new(u16::MAX)).collect::<Vec<_>>();
        (0..n)
            .into_par_iter()
            .with_min_len(4096)
            .for_each_init(Vec::new, |stack, i| {
                if flags[i] & DIR_IN_USE != DIR_IN_USE {
                    return;
                }
                let known = |r: usize| depth[r].load(Ordering::Relaxed);
                if known(i) != u16::MAX {
                    return;
                }
                stack.clear();
                let mut cur = i;
                let base = loop {
                    if cur == ROOT_RECORD as usize
                        || !is_dir(parent[cur])
                        || stack.len() > MAX_DEPTH
                    {
                        break 0;
                    }
                    if known(cur) != u16::MAX {
                        break known(cur);
                    }
                    stack.push(cur);
                    cur = parent[cur] as usize;
                };
                if known(cur) == u16::MAX {
                    depth[cur].store(base, Ordering::Relaxed);
                }
                let mut d = known(cur);
                while let Some(s) = stack.pop() {
                    d = d.saturating_add(1);
                    depth[s].store(d, Ordering::Relaxed);
                }
            });

        // Add each directory to its parent, deepest first; one level at a time, its directories
        // in parallel
        let depth = depth
            .into_iter()
            .map(AtomicU16::into_inner)
            .collect::<Vec<_>>();
        let max_depth = depth
            .par_iter()
            .copied()
            .filter(|&d| d != u16::MAX)
            .max()
            .unwrap_or(0);
        let mut by_depth = vec![Vec::new(); max_depth as usize + 1];
        for (i, &d) in depth.iter().enumerate() {
            if d != u16::MAX && d > 0 {
                by_depth[d as usize].push(i as u32);
            }
        }
        for dirs in by_depth.iter().rev() {
            dirs.par_iter().with_min_len(1024).for_each(|&d| {
                let p = parent[d as usize];
                if p != d && is_dir(p) {
                    let below = size[d as usize].load(Ordering::Relaxed);
                    size[p as usize].fetch_add(below, Ordering::Relaxed);
                }
            });
        }
    }

    /// Adds `delta` to the sizes of all directories above `record`, through each of its names.
    pub fn add_to_ancestors(&mut self, record: u32, delta: i64) {
        if delta == 0 {
            return;
        }
        for id in self.entries_of(record) {
            let mut cur = self.parent(id);
            for _ in 0..MAX_DEPTH {
                if !self.is_live_dir(cur) || cur == record {
                    break;
                }
                let s = &mut self.records.size[cur as usize];
                *s = s.saturating_add_signed(delta);
                if cur == ROOT_RECORD {
                    break;
                }
                cur = self.records.parent[cur as usize];
            }
        }
    }
}

/// The same memory as atomics, for adding to elements from several threads.
fn atomics(values: &mut [u64]) -> &[AtomicU64] {
    const _: () = assert!(align_of::<AtomicU64>() == align_of::<u64>());
    // SAFETY: same size and alignment, and the exclusive borrow rules out other accesses
    unsafe { &*(values as *mut [u64] as *const [AtomicU64]) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{Records, FLAG_HAS_LINKS};
    use crate::ntfs::volume::Volume;

    /// The sequential computation the parallel one replaced
    fn folder_sizes_sequential(index: &mut VolumeIndex) {
        let n = index.records.len();
        let flags = &index.records.flags;
        let parent = &index.records.parent;
        let size = &mut index.records.size;

        for i in 0..n {
            if flags[i] & DIR_IN_USE == DIR_IN_USE {
                size[i] = 0;
            }
        }

        // Direct contributions of files and their hard links
        let is_dir = |r: u32| {
            flags
                .get(r as usize)
                .is_some_and(|f| f & DIR_IN_USE == DIR_IN_USE)
        };
        for i in 0..n {
            if flags[i] & DIR_IN_USE == FLAG_IN_USE && is_dir(parent[i]) {
                size[parent[i] as usize] += size[i];
            }
        }
        for (l, &r) in index.links.record.iter().enumerate() {
            let p = index.links.parent[l];
            if r != NO_RECORD && flags[r as usize] & DIR_IN_USE == FLAG_IN_USE && is_dir(p) {
                size[p as usize] += size[r as usize];
            }
        }

        // Depth of every directory, then add each directory to its parent, deepest first
        let mut depth = vec![u16::MAX; n];
        let mut stack = Vec::new();
        for i in 0..n {
            if flags[i] & DIR_IN_USE != DIR_IN_USE || depth[i] != u16::MAX {
                continue;
            }
            let mut cur = i;
            let base = loop {
                if cur == ROOT_RECORD as usize || !is_dir(parent[cur]) || stack.len() > MAX_DEPTH {
                    break 0;
                }
                if depth[cur] != u16::MAX {
                    break depth[cur];
                }
                stack.push(cur);
                cur = parent[cur] as usize;
            };
            if depth[cur] == u16::MAX {
                depth[cur] = base;
            }
            let mut d = depth[cur];
            while let Some(s) = stack.pop() {
                d = d.saturating_add(1);
                depth[s] = d;
            }
        }

        let max_depth = depth
            .iter()
            .filter(|&&d| d != u16::MAX)
            .max()
            .copied()
            .unwrap_or(0);
        let mut by_depth = vec![Vec::new(); max_depth as usize + 1];
        for (i, &d) in depth.iter().enumerate() {
            if d != u16::MAX && d > 0 {
                by_depth[d as usize].push(i as u32);
            }
        }
        for dirs in by_depth.iter().rev() {
            for &d in dirs {
                let p = parent[d as usize];
                if p as usize != d as usize && is_dir(p) {
                    size[p as usize] += size[d as usize];
                }
            }
        }
    }

    /// Random trees with files, hard links and broken parents give the same sizes as before
    #[test]
    fn parallel_matches_sequential() {
        let mut x = 0x2545f4914f6cdd1d_u64;
        let mut next = move |max: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % max
        };
        for n in [16, 100, 20_000, 200_000] {
            let mut index = VolumeIndex::empty(Volume { id: 'T' });
            index.records = Records::with_len(n);
            let r = &mut index.records;
            r.flags[ROOT_RECORD as usize] = FLAG_IN_USE | FLAG_DIRECTORY;
            r.parent[ROOT_RECORD as usize] = ROOT_RECORD;
            for i in 16..n {
                r.flags[i] = match next(10) {
                    0..=2 => FLAG_IN_USE | FLAG_DIRECTORY,
                    3 => 0,
                    _ => FLAG_IN_USE,
                };
                // Mostly an earlier record, which may be a file, a free record or the root
                r.parent[i] = if next(50) == 0 {
                    next(n as u64) as u32
                } else {
                    next(i as u64).max(5) as u32
                };
                r.size[i] = next(1 << 40);
            }
            for _ in 0..n / 50 {
                let (record, parent) = (16 + next(n as u64 - 16) as u32, next(n as u64) as u32);
                index.records.flags[record as usize] |= FLAG_HAS_LINKS;
                index.links.push(record, parent, 0, 0);
            }
            let mut expected = VolumeIndex::empty(Volume { id: 'T' });
            expected.records = index.records.with_room(n);
            expected.links = std::mem::take(&mut index.links);
            index.links.record = expected.links.record.clone();
            index.links.parent = expected.links.parent.clone();
            folder_sizes_sequential(&mut expected);
            index.compute_folder_sizes();
            assert_eq!(index.records.size, expected.records.size, "{} records", n);
        }
    }
}
